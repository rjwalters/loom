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
//!   paginated, with `S` = [`window_opens`]: now − [`WINDOW_DAYS`] (the
//!   14-day fit window plus margin), aligned down to UTC midnight. More than
//!   [`MAX_PAGES`] pages is an error, not a silent cut.
//! - `GET repos/{store}/commits?path=repos.yml&sha={ref}&until={S}&per_page=1`:
//!   the **anchor**, the revision in force when the window opens.
//! - `GET repos/{store}/contents/repos.yml?ref={commit}` for each commit not
//!   already cached. Contents are stored content-addressed by blob SHA, and
//!   a commit is immutable, so no revision is fetched twice.
//!
//! Requests go through the store's [`Transport`]: in production
//! `fleet_store::gh::GhTransport`, the reader App first (then the writer App,
//! then ambient `gh` auth on a standalone install). No operator token, and no
//! new credential.
//!
//! # Forge budget: conditional listings
//!
//! Because `S` is aligned to the UTC day, each listing URL repeats on every
//! poll of that day. The index keeps each listing's `ETag` and the commits it
//! listed ([`CachedListing`]). The next poll sends `If-None-Match`, and a
//! `304` reuses the cached commits; a `304` with nothing cached is an error.
//! On GitHub an authorized `304` does not count against the primary rate
//! limit. So with `repos.yml` unchanged, a poll costs two conditional
//! requests (the first page and the anchor) and no contents. A poll that
//! answers `304` is still a successful poll, and moves `last_poll_at`. Each
//! new UTC day opens with one unconditional listing.
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
//! # The observation archive
//!
//! The index is rewritten on every poll and pruned to the window, so on its
//! own an observation would be lost when its commit ages out, or when the
//! index is lost. Every commit's first sighting is therefore also appended to
//! `observations.jsonl` ([`Observation`]) before the index is written. The
//! archive is append-only and never pruned: one line per store, ref and
//! commit, written once. It is the durable record of when this host first
//! saw each revision, for backtests whose cutoffs are older than the window
//! ([`archive`]).
//!
//! [`sync`] gives a listed commit its observation from the previous index
//! when it has one, else from the archive. Only a commit in neither is
//! *new*. So an index that is lost or unreadable does not re-date a commit
//! that was already archived. An archive line for the store and ref is also
//! proof that an earlier poll succeeded, because a line is written only by a
//! poll whose listing succeeded. So after an index loss, a new commit is
//! still observed at the poll that first lists it, exactly as with the
//! index. A corrupt archive line is skipped. It is never read as an
//! observation.
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

/// The append-only observation archive, inside the cache directory.
pub const ARCHIVE: &str = "observations.jsonl";

/// The cache directory: `<root>/.loom/state/eta/roster-history`, beside the
/// fleet snapshots and the fit files. It is ignored state. The index and the
/// blobs can be regenerated from the forge; the [`ARCHIVE`] cannot, because
/// it records when this host saw each commit.
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
    /// The last poll's listings that carried an `ETag`, for conditional
    /// re-requests. Absent from an index written before they existed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub listings: Vec<CachedListing>,
}

/// One commit as a listing named it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListedEntry {
    /// The commit SHA.
    pub sha: String,
    /// Its committer date.
    pub committed_at: DateTime<Utc>,
}

/// A listing request's last `200` answer, for [`sync`]'s conditional
/// re-request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedListing {
    /// The REST path, query included.
    pub path: String,
    /// The `ETag` the forge returned for it.
    pub etag: String,
    /// The commits it listed, as the forge ordered them (newest first).
    pub commits: Vec<ListedEntry>,
}

/// One line of the [`ARCHIVE`]: a commit's first sighting by this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// The store, `OWNER/REPO`.
    pub repo: String,
    /// The ref read.
    pub reference: String,
    /// The commit SHA.
    pub sha: String,
    /// Its committer date.
    pub committed_at: DateTime<Utc>,
    /// The blob SHA of its `repos.yml`.
    pub blob: String,
    /// The poll that first listed it.
    pub first_listed_at: DateTime<Utc>,
    /// Its observation (see the module docs). `None` when the poll that
    /// first listed it was the first one for this store and ref.
    #[serde(default)]
    pub observed_at: Option<DateTime<Utc>>,
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

/// The listings cached by the previous poll, by path.
type Cached<'a> = BTreeMap<&'a str, &'a CachedListing>;

/// One listing request: conditional when `cached` has its path, and a `304`
/// then answers with the cached commits. Returns the commits (newest first)
/// and what to cache for the next poll.
fn list(
    transport: &dyn Transport,
    path: &str,
    cached: &Cached<'_>,
) -> Result<(Vec<ListedEntry>, Option<CachedListing>)> {
    let prior = cached.get(path).copied();
    let reply = transport.get(path, None, prior.map(|c| c.etag.as_str()))?;
    if reply.status == 304 {
        let Some(prior) = prior else {
            bail!("forge answered 304 Not Modified listing {path}, but nothing is cached for it");
        };
        return Ok((prior.commits.clone(), Some(prior.clone())));
    }
    if reply.status != 200 {
        bail!("forge answered HTTP {} listing {path}", reply.status);
    }
    let listed: Vec<ListedCommit> =
        serde_json::from_str(&reply.body).context("malformed commit listing")?;
    let commits = listed
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
            Ok(ListedEntry {
                sha: c.sha,
                committed_at: at,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let cache = reply
        .etag
        .filter(|e| !e.is_empty())
        .map(|etag| CachedListing {
            path: path.to_string(),
            etag,
            commits: commits.clone(),
        });
    Ok((commits, cache))
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

/// When the listed window opens at `now`: [`WINDOW_DAYS`] back, aligned
/// down to UTC midnight, so the listing URLs repeat all day (see the module
/// docs).
#[must_use]
pub fn window_opens(now: DateTime<Utc>) -> DateTime<Utc> {
    let start = now - Duration::days(WINDOW_DAYS);
    let secs = start.timestamp();
    DateTime::from_timestamp(secs - secs.rem_euclid(86_400), 0).unwrap_or(start)
}

/// Poll the store and bring the cache in `dir` up to date (see the module
/// docs). Any failure is an error and leaves the previous index intact, and
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
    let archived: BTreeMap<String, Observation> = archive(dir, location)
        .into_iter()
        .map(|o| (o.sha.clone(), o))
        .collect();
    // Either one proves an earlier poll of this store and ref succeeded.
    let polled_before = previous.is_some() || !archived.is_empty();
    let cached: Cached<'_> = previous
        .as_ref()
        .map(|i| i.listings.iter().map(|l| (l.path.as_str(), l)).collect())
        .unwrap_or_default();
    let since = window_opens(now).to_rfc3339_opts(SecondsFormat::Secs, true);
    let base =
        format!("repos/{}/commits?path={ROSTER_PATH}&sha={}", location.repo, location.reference);
    // Newest first, as the forge lists them.
    let mut listed = Vec::new();
    let mut listings = Vec::new();
    for page in 1..=MAX_PAGES + 1 {
        if page > MAX_PAGES {
            bail!(
                "{ROSTER_PATH} has more than {} commits in {WINDOW_DAYS} days",
                MAX_PAGES * PER_PAGE
            );
        }
        let path = format!("{base}&since={since}&per_page={PER_PAGE}&page={page}");
        let (batch, cache) = list(transport, &path, &cached)?;
        listings.extend(cache);
        let n = batch.len();
        listed.extend(batch);
        if n < PER_PAGE {
            break;
        }
    }
    let (anchor, cache) = list(transport, &format!("{base}&until={since}&per_page=1"), &cached)?;
    listings.extend(cache);
    for a in anchor {
        if !listed.iter().any(|l| l.sha == a.sha) {
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
    let mut sightings = Vec::new();
    for ListedEntry { sha, committed_at } in listed {
        let old = known.get(sha.as_str()).copied();
        let kept = archived.get(&sha);
        let entry = match old.filter(|o| blob_dir.join(&o.blob).is_file()) {
            Some(o) => o.clone(),
            None => {
                let (blob, body) = fetch_contents(transport, location, &sha)?;
                write_atomic(&blob_dir.join(&blob), &body)?;
                report.fetched += 1;
                let observed_at = match (old, kept) {
                    (Some(o), _) => o.observed_at,
                    (None, Some(a)) => a.observed_at,
                    (None, None) => {
                        report.new += 1;
                        polled_before.then_some(now)
                    }
                };
                CommitEntry {
                    sha,
                    committed_at,
                    observed_at,
                    blob,
                }
            }
        };
        if kept.is_none() {
            // A commit carried over from an index older than the archive was
            // listed no later than its observation, or than that index's poll.
            let first_listed_at = old
                .and_then(|o| o.observed_at)
                .or_else(|| old.and(previous.as_ref().map(|i| i.last_poll_at)))
                .unwrap_or(now);
            sightings.push(Observation {
                repo: location.repo.clone(),
                reference: location.reference.clone(),
                sha: entry.sha.clone(),
                committed_at: entry.committed_at,
                blob: entry.blob.clone(),
                first_listed_at,
                observed_at: entry.observed_at,
            });
        }
        commits.push(entry);
    }
    report.revisions = commits.len();
    // The archive first: the index never records an observation the archive
    // does not hold.
    append_archive(dir, &sightings)?;
    let index = Index {
        version: INDEX_VERSION,
        repo: location.repo.clone(),
        reference: location.reference.clone(),
        last_poll_at: now,
        commits,
        listings,
    };
    let mut body = serde_json::to_vec_pretty(&index)?;
    body.push(b'\n');
    write_atomic(&dir.join(INDEX), &body)?;
    prune_blobs(&blob_dir, &index);
    Ok(report)
}

/// The [`ARCHIVE`]'s observations of `location`'s store and ref, in the order
/// they were written, one per commit (the first line wins). A line that does
/// not parse is skipped. Disk only; an absent archive is empty.
#[must_use]
pub fn archive(dir: &Path, location: &StoreLocation) -> Vec<Observation> {
    let Ok(raw) = std::fs::read_to_string(dir.join(ARCHIVE)) else {
        return Vec::new();
    };
    let mut seen = std::collections::BTreeSet::new();
    raw.lines()
        .filter_map(|line| serde_json::from_str::<Observation>(line).ok())
        .filter(|o| {
            o.repo == location.repo
                && o.reference == location.reference
                && is_sha(&o.sha)
                && seen.insert(o.sha.clone())
        })
        .collect()
}

/// Append `lines` to the [`ARCHIVE`] in one write. A torn last line from an
/// earlier crash is closed off first, so it stays one skipped line.
fn append_archive(dir: &Path, lines: &[Observation]) -> Result<()> {
    use std::io::Write as _;
    if lines.is_empty() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let path = dir.join(ARCHIVE);
    let mut out = Vec::new();
    if std::fs::read(&path).is_ok_and(|b| b.last().is_some_and(|&c| c != b'\n')) {
        out.push(b'\n');
    }
    for line in lines {
        serde_json::to_writer(&mut out, line)?;
        out.push(b'\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("could not open {}", path.display()))?;
    file.write_all(&out)
        .and_then(|()| file.sync_all())
        .with_context(|| format!("could not append to {}", path.display()))
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
