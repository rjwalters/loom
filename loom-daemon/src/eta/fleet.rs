//! Fleet-wide, forge-derived stage-boundary history (#9343).
//!
//! # The problem this exists for
//!
//! [`super::history`]'s local producer reads one host's journals, so a daemon
//! estimates only from the sweeps *it* ran: empty on a host that ran none,
//! small-n and hardware-shaped where it ran some, different on every host, and
//! blind to the human-gated stages (Judge wait, Doctor turnaround, merge wait)
//! that dominate lead time and happen on the forge rather than on any host.
//!
//! # What this module is
//!
//! One **snapshot** per repo ([`FleetSnapshot`]): every post-dispatch stage
//! boundary the repo's PR label timelines record, in a canonical order, under a
//! content-derived id. Two hosts holding the same snapshot id hold the same
//! bytes, so they estimate identically — the determinism requirement on #9343.
//!
//! # Three properties, and where each is enforced
//!
//! **Purity.** Nothing here touches the forge. [`FleetSnapshot::merge`] takes
//! already-fetched [`PrHistory`] values, exactly as `pr_latency` does, and the
//! `gh` reads live in `cli/eta_fleet_cmd.rs`. An estimator therefore still sees
//! only `(snapshot, input)` and #9325's backtest harness can replay a fleet
//! snapshot with no behaviour change to the estimator's core.
//!
//! **Determinism.** Samples are stored in a canonical order and deduplicated;
//! [`FleetSnapshot::snapshot_id`] is derived (never random, per the
//! trace-identity policy) from the repo, the as-of instant and a digest of
//! every sample. Nothing host-shaped is carried: the derivation deliberately
//! drops the [`Provenance`] `journal`'s row builder stamps, so two hosts on
//! different Loom builds produce the same bytes.
//!
//! **Leak-freedom.** Every sample carries the instant it became observable —
//! the forge event's own timestamp, not the time it was fetched — and
//! [`super::history::StageSamples::select`] refuses samples at or after the
//! estimate's `as_of`. A snapshot taken today can therefore be replayed at any
//! earlier instant without leaking its own future.
//!
//! # Cost: backfill once, then incremental
//!
//! The first build reads one timeline per PR (#9343's evidence section measured
//! 46 merged PRs for one repo's backfill). After that
//! [`FleetSnapshot::cursor`] bookmarks the newest forge activity already
//! ingested, so a refresh enumerates only PRs updated since — typically a
//! handful — and [`FleetSnapshot::merge`] replaces that PR's rows in place.
//! The daemon itself never derives: it reads the cached file
//! ([`load_all`]) and makes no forge call at all.
//!
//! # Stage episodes (#10218)
//!
//! Next to the samples, a snapshot holds every PR's **stage episodes**
//! ([`super::episodes`]), derived from the same timelines with no further
//! forge read: the split stage path (`merge_wait → merge_hold → merge_wait →
//! merged`) a hold-aware heuristic fits. They are a separate field, never
//! samples, so an older daemon still parses the file, and the pooled
//! `merge_wait` samples every shipped heuristic reads are untouched. A snapshot
//! written before #10218 gains episodes only as its PRs are re-read: run
//! `eta fleet backfill` (not `refresh`) to rebuild the whole window.
//!
//! # Deliberately out of scope
//!
//! Fleet-wide **in-sweep** samples (`sweep.curator`, `sweep.builder`) would
//! have to come from the fleet's `sweep.outcome` records in SigNoz, which is
//! blocked on fleet workers exporting at all (harness-ops#249). A forge
//! timeline cannot see inside a sweep, so a fleet-only history supports `land`
//! (whose path is `review_wait` → `doctor` → `merge_wait`) and refuses
//! `finish` for want of phase samples — correctly, as a refusal rather than a
//! guess.

use super::config::HistoryScopeMode;
use super::episodes::{episodes_from_pr_history, StageEpisode};
use super::explanation::HistoryScope;
use super::history::{SampleSource, StageSample, StageSamples, VerdictSample};
use super::journal::{censored_from_pr_history, entries_from_pr_history, JournalEntry};
use super::{Provenance, Stage, WINDOW_DAYS};
use crate::pr_latency::PrHistory;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Schema tag of one snapshot file.
pub const SNAPSHOT_SCHEMA: &str = "eta-fleet-snapshot/v1";

/// The `samples_by_host` attribution of a forge-derived sample.
///
/// Not a host id and not meant to look like one: no host recorded it. Keeping
/// it distinct from every real `host_id` is what lets forensics read off an
/// explanation how much of an estimate was fleet evidence and how much was the
/// estimating host's own.
pub const FORGE_HOST: &str = "forge";

/// Test seam: overrides the directory snapshots are cached in.
pub const SNAPSHOT_DIR_ENV: &str = "LOOM_ETA_FLEET_SNAPSHOT_DIR";

/// How long a sample is kept in the snapshot file, in days.
///
/// Twice [`WINDOW_DAYS`], not once: an estimate at `as_of` reads one window
/// back from `as_of`, so a backtest replaying an instant a window ago needs a
/// second window behind *that*. Pruning at exactly [`WINDOW_DAYS`] would make
/// the cache file silently unusable for replay.
pub const RETENTION_DAYS: i64 = WINDOW_DAYS * 2;

/// One forge-derived stage boundary: a `(repo, stage)` duration read off one
/// PR's label timeline.
///
/// Every field is a forge fact. There is no host, no build and no fetch time in
/// here, which is exactly why two hosts deriving from the same timelines get
/// identical bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSample {
    /// `owner/repo`.
    pub repo: String,
    /// The PR whose timeline it came from.
    pub pr_number: u32,
    /// The stage.
    pub stage: Stage,
    /// The label event that opened the stage.
    pub entered_at: DateTime<Utc>,
    /// The forge event that closed it — or, for a censored sample, the instant
    /// the timeline was read at. Also the instant the sample became
    /// observable, which is what keeps a replay leak-free.
    pub observed_at: DateTime<Utc>,
    /// Whole seconds. A **lower bound** when `censored`.
    pub duration_sec: i64,
    /// The stage had not closed when the snapshot was taken (#9328): this is a
    /// lower bound, read only by a censoring heuristic (`land-v2`).
    #[serde(default)]
    pub censored: bool,
    /// `pass` / `fail` when this row is also a Judge verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// 1-based attempt the verdict settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<u32>,
}

impl FleetSample {
    /// The canonical sort key. Total over the fields a snapshot stores, so the
    /// order does not depend on the order PRs were fetched in.
    fn key(&self) -> (String, Stage, u32, DateTime<Utc>, DateTime<Utc>, i64, bool) {
        (
            self.repo.to_ascii_lowercase(),
            self.stage,
            self.pr_number,
            self.entered_at,
            self.observed_at,
            self.duration_sec,
            self.censored,
        )
    }

    /// The one-line canonical form the snapshot id is digested over.
    fn digest_line(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}",
            self.repo.to_ascii_lowercase(),
            self.stage.as_str(),
            self.pr_number,
            crate::telemetry::trace::instant(self.entered_at),
            crate::telemetry::trace::instant(self.observed_at),
            self.duration_sec,
            u8::from(self.censored),
            self.verdict.as_deref().unwrap_or(""),
            self.attempt.map(|a| a.to_string()).unwrap_or_default(),
        )
    }

    /// This sample as history, attributed to [`FORGE_HOST`] and
    /// [`SampleSource::ForgeTimeline`].
    fn stage_sample(&self) -> StageSample {
        StageSample {
            repo: self.repo.clone(),
            stage: self.stage,
            duration_sec: self.duration_sec,
            observed_at: self.observed_at,
            source: SampleSource::ForgeTimeline,
            host: FORGE_HOST.to_string(),
            // #9420: a forge label timeline is worked-only by construction —
            // a `loom:review-requested` → verdict interval exists because the
            // transition happened — so there is nothing to condition on.
            worked: None,
        }
    }

    /// The Judge verdict this sample records, when it records one.
    fn verdict_sample(&self) -> Option<VerdictSample> {
        if self.censored {
            return None;
        }
        let rejected = match self.verdict.as_deref()? {
            "fail" => true,
            "pass" => false,
            _ => return None,
        };
        Some(VerdictSample {
            repo: self.repo.clone(),
            attempt: self.attempt?,
            rejected,
            observed_at: self.observed_at,
        })
    }
}

/// A fleet-wide stage-boundary history for one repo, as of one instant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetSnapshot {
    /// Always [`SNAPSHOT_SCHEMA`].
    pub schema: String,
    /// Derived, never random: a function of `(repo, as_of, every sample)`.
    /// Two hosts holding this id hold the same samples.
    pub snapshot_id: String,
    /// `owner/repo`.
    pub repo: String,
    /// The instant the snapshot describes: no sample in it was observed after
    /// this, and a censored sample is censored *at* it.
    pub as_of: DateTime<Utc>,
    /// Incremental bookmark: the newest forge activity already ingested. A
    /// refresh enumerates only PRs updated at or after this, so the full
    /// derivation is paid once. `None` on an empty snapshot.
    pub cursor: Option<DateTime<Utc>>,
    /// PR numbers ingested, ascending — the census, and what makes a re-read
    /// of one PR replace its rows rather than duplicate them.
    pub prs: Vec<u32>,
    /// Every sample, in canonical order.
    pub samples: Vec<FleetSample>,
    /// Every PR's stage episodes (#10218), in canonical order
    /// ([`StageEpisode::key`]).
    ///
    /// Deliberately not samples: an older daemon ignores this unknown field
    /// and still parses the file, whereas a `merge_hold` [`FleetSample`] would
    /// be an unknown `stage` and make it refuse the whole snapshot.
    /// `serde(default)` reads files written before #10218, and an empty list
    /// is not written, so such a file round-trips byte-identically.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub episodes: Vec<StageEpisode>,
}

impl FleetSnapshot {
    /// An empty snapshot for `repo`.
    #[must_use]
    pub fn empty(repo: &str) -> Self {
        let mut snapshot = FleetSnapshot {
            schema: SNAPSHOT_SCHEMA.to_string(),
            snapshot_id: String::new(),
            repo: repo.to_string(),
            as_of: DateTime::<Utc>::MIN_UTC,
            cursor: None,
            prs: Vec::new(),
            samples: Vec::new(),
            episodes: Vec::new(),
        };
        snapshot.seal();
        snapshot
    }

    /// Fold `histories` in, as read at `as_of`.
    ///
    /// Idempotent per PR: every sample already held for a PR in `histories` is
    /// dropped first, so re-reading a PR (which a refresh does for anything
    /// that moved) replaces its rows instead of duplicating them. That is also
    /// what makes a censored row correct over time — an open `merge_wait`
    /// recorded yesterday is replaced by the completed one once it merges.
    ///
    /// A PR whose timeline read did not fully answer
    /// ([`PrHistory::timeline_complete`]) contributes nothing **and** does not
    /// evict what is already held: a partial log would otherwise silently
    /// shorten a stage, which is the one failure mode `pr_latency` was built to
    /// refuse.
    ///
    /// Samples older than [`RETENTION_DAYS`] before `as_of` are pruned, so the
    /// cache file does not grow without bound. Episodes follow the same rules:
    /// replaced per re-read PR, pruned by the last instant they describe.
    pub fn merge(&mut self, histories: &[PrHistory], as_of: DateTime<Utc>) {
        let usable: Vec<&PrHistory> = histories.iter().filter(|h| h.timeline_complete).collect();
        let replaced: BTreeSet<u32> = usable.iter().map(|h| h.number).collect();
        self.samples.retain(|s| !replaced.contains(&s.pr_number));
        self.episodes.retain(|e| !replaced.contains(&e.pr_number));
        for h in &usable {
            self.samples
                .extend(samples_from_pr_history(h, &self.repo, as_of));
            self.episodes
                .extend(episodes_from_pr_history(h, &self.repo, as_of));
        }
        self.as_of = self.as_of.max(as_of);
        // `checked_sub_signed`, not `-`: chrono **panics** on overflow, and
        // `FleetSnapshot::empty` starts at `MIN_UTC`. No floor at all is the
        // right fallback — it prunes nothing, which is what a snapshot that
        // cannot be a retention window old deserves.
        if let Some(floor) = self
            .as_of
            .checked_sub_signed(Duration::days(RETENTION_DAYS))
        {
            self.samples.retain(|s| s.observed_at >= floor);
            self.episodes.retain(|e| e.last_at() >= floor);
        }
        let mut prs: BTreeSet<u32> = self.prs.iter().copied().collect();
        prs.extend(replaced);
        // A PR that has aged entirely out of the window is no longer described
        // by this snapshot; keeping it in the census would overstate coverage.
        self.prs = prs
            .into_iter()
            .filter(|pr| {
                self.samples.iter().any(|s| s.pr_number == *pr)
                    || self.episodes.iter().any(|e| e.pr_number == *pr)
            })
            .collect();
        self.seal();
    }

    /// Canonicalize (sort, dedup), recompute the cursor, and derive the id.
    /// Every mutating path ends here, so a snapshot value is never observed
    /// out of canonical form.
    fn seal(&mut self) {
        self.schema = SNAPSHOT_SCHEMA.to_string();
        self.samples.sort_by_key(FleetSample::key);
        self.samples.dedup();
        self.episodes.sort_by(|a, b| {
            a.key()
                .cmp(&b.key())
                .then_with(|| a.digest_line().cmp(&b.digest_line()))
        });
        self.episodes.dedup();
        self.cursor = self.samples.iter().map(|s| s.observed_at).max();
        let mut lines: Vec<String> = self.samples.iter().map(FleetSample::digest_line).collect();
        // #10218: episode lines only when there are episodes (each starts
        // `episode|`, so it cannot read as a sample line), so a snapshot with
        // none keeps exactly the id it had before episodes existed.
        lines.extend(self.episodes.iter().map(StageEpisode::digest_line));
        let mut parts: Vec<&str> = vec!["loom.eta.fleet.snapshot"];
        let repo = self.repo.to_ascii_lowercase();
        let at = crate::telemetry::trace::instant(self.as_of);
        parts.push(&repo);
        parts.push(&at);
        parts.extend(lines.iter().map(String::as_str));
        self.snapshot_id = crate::telemetry::trace::derived_hex(&parts, 16);
    }

    /// This snapshot as estimator input: `scope = Fleet`, every sample
    /// attributed to [`SampleSource::ForgeTimeline`] and [`FORGE_HOST`].
    ///
    /// Censored samples land in [`StageSamples::censored`], never in `stages`,
    /// so no v1 heuristic's distribution can change shape because a snapshot
    /// was handed to it.
    ///
    /// `paths` stays empty: whether a sweep merged its own PR is a
    /// sweep-internal fact the forge cannot see, so a fleet-only history has no
    /// merge share and `finish-v1` refuses on it rather than inventing one.
    /// `land-v1`/`land-v2` are unaffected (`always_merge`).
    ///
    /// The episodes (#10218) are copied into [`StageSamples::episodes`], and
    /// each `merge_hold` episode also becomes a `merge_hold` sample (completed
    /// when it left for a stage or a merge, censored otherwise) so
    /// `select(repo, MergeHold, …)` answers. The split `merge_wait` episodes
    /// are deliberately **not** samples: `merge_wait` samples keep the pooled
    /// definition every shipped heuristic reads.
    #[must_use]
    pub fn stage_samples(&self) -> StageSamples {
        let mut history = StageSamples {
            scope: HistoryScope::Fleet,
            ..StageSamples::default()
        };
        for sample in &self.samples {
            if sample.censored {
                history.censored.push(sample.stage_sample());
            } else {
                history.stages.push(sample.stage_sample());
            }
            if let Some(verdict) = sample.verdict_sample() {
                history.verdicts.push(verdict);
            }
        }
        for episode in self.episodes.iter().filter(|e| e.stage == Stage::MergeHold) {
            let sample = StageSample {
                repo: episode.repo.clone(),
                stage: Stage::MergeHold,
                duration_sec: episode.duration_sec(),
                observed_at: episode.last_at(),
                source: SampleSource::ForgeTimeline,
                host: FORGE_HOST.to_string(),
                worked: None,
            };
            if episode.completed() {
                history.stages.push(sample);
            } else {
                history.censored.push(sample);
            }
        }
        history.episodes = self.episodes.clone();
        history
    }

    /// Episode counts per stage name, `(completed, lower bounds)` — the
    /// `eta fleet show` census of the split stage record (#10218). A lower
    /// bound is an episode still open, cut short by a close, or unstaged.
    #[must_use]
    pub fn episode_counts_by_stage(&self) -> BTreeMap<String, (usize, usize)> {
        let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for episode in &self.episodes {
            let entry = counts
                .entry(episode.stage.as_str().to_string())
                .or_default();
            if episode.completed() {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
        counts
    }

    /// Sample counts per stage name, completed and censored — the `eta fleet
    /// show` census, and what tells an operator whether a repo has enough
    /// evidence for `land` to answer at all.
    #[must_use]
    pub fn counts_by_stage(&self) -> BTreeMap<String, (usize, usize)> {
        let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for sample in &self.samples {
            let entry = counts.entry(sample.stage.as_str().to_string()).or_default();
            if sample.censored {
                entry.1 += 1;
            } else {
                entry.0 += 1;
            }
        }
        counts
    }

    /// Judge verdicts in the snapshot, `(total, rejected)`.
    #[must_use]
    pub fn verdict_counts(&self) -> (usize, usize) {
        let verdicts: Vec<VerdictSample> = self
            .samples
            .iter()
            .filter_map(FleetSample::verdict_sample)
            .collect();
        (verdicts.len(), verdicts.iter().filter(|v| v.rejected).count())
    }
}

/// The derivation below reads no build information — [`JournalEntry::loom`] is
/// dropped on the way to [`FleetSample`] — so a fixed placeholder is used
/// rather than [`Provenance::current`]. Two hosts on different Loom builds must
/// derive byte-identical snapshots, and stamping the running build here is the
/// obvious way to break that.
fn derivation_provenance() -> Provenance {
    Provenance {
        version: env!("CARGO_PKG_VERSION").to_string(),
        revision: "unknown".to_string(),
        tree_state: "unknown".to_string(),
        complete: false,
    }
}

/// Every fleet sample one PR's forge history contributes, as read at `as_of`.
///
/// Deliberately **not** a second derivation: it reuses
/// [`entries_from_pr_history`] and [`censored_from_pr_history`], the same
/// functions `eta backfill` (#9325) writes into the local stage journal with,
/// and re-attributes their rows to the forge. A fleet snapshot and a local
/// backfill of the same PR therefore describe the same segments — the entry /
/// re-application rules, the attempt numbering and the open-segment handling
/// all live in one place and cannot drift.
#[must_use]
pub fn samples_from_pr_history(
    h: &PrHistory,
    repo: &str,
    as_of: DateTime<Utc>,
) -> Vec<FleetSample> {
    let loom = derivation_provenance();
    let mut samples = Vec::new();
    for row in entries_from_pr_history(h, repo, &loom) {
        if let Some(sample) = completed_sample(&row, repo) {
            samples.push(sample);
        }
    }
    for row in censored_from_pr_history(h, repo, as_of, &loom) {
        if let Some(sample) = censored_sample(&row, repo) {
            samples.push(sample);
        }
    }
    // Leak-freedom is a property of the *estimate*, not of the file, but a row
    // observed after the snapshot's own as-of has no business being in it: it
    // could only come from a clock skew or a caller passing a past `as_of`.
    samples.retain(|s| s.observed_at <= as_of);
    samples.sort_by_key(FleetSample::key);
    samples
}

fn completed_sample(row: &JournalEntry, repo: &str) -> Option<FleetSample> {
    let duration_sec = row.duration_sec?;
    Some(FleetSample {
        repo: repo.to_string(),
        pr_number: row.pr_number?,
        stage: row.stage?,
        entered_at: row.entered_at?,
        observed_at: row.observed_at,
        duration_sec: duration_sec.max(0),
        censored: false,
        verdict: row.verdict.clone(),
        attempt: row.attempt,
    })
}

fn censored_sample(row: &JournalEntry, repo: &str) -> Option<FleetSample> {
    let censored_sec = row.censored_sec?;
    Some(FleetSample {
        repo: repo.to_string(),
        pr_number: row.pr_number?,
        stage: row.stage?,
        entered_at: row.entered_at?,
        observed_at: row.observed_at,
        duration_sec: censored_sec.max(0),
        censored: true,
        verdict: None,
        attempt: None,
    })
}

/// The cache directory snapshots live in: `<root>/.loom/state/eta/fleet`, or
/// [`SNAPSHOT_DIR_ENV`] when set.
///
/// `.loom/state/` and not a new `.loom/cache/`: `state/*` is ignored
/// **wholesale** by the managed `.gitignore` block (#9592), and the comment
/// there records what shipping a new daemon-written path outside it costs — a
/// host committed `.loom/state/eta/pending.jsonl`, which kept that clone
/// permanently dirty and got fleet resyncs from it refused as downgrades. This
/// is per-host, daemon-written, regenerable state in exactly that class, so it
/// belongs under the same already-ignored root as the ETA subsystem's other
/// two files (`pending.jsonl`, `shadow.json`).
///
/// The `fleet/` leaf keeps [`load_all`]'s directory listing to snapshots
/// alone, so a sibling like `shadow.json` can never be parsed as one.
#[must_use]
pub fn snapshot_dir(workspace_root: &Path) -> PathBuf {
    match std::env::var(SNAPSHOT_DIR_ENV) {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => workspace_root
            .join(".loom")
            .join("state")
            .join("eta")
            .join("fleet"),
    }
}

/// The file name component for `repo`: lowercased, with every character that
/// is not `[a-z0-9._-]` folded to `-`, so a slug can never escape the cache
/// directory or collide with the `.json` suffix.
#[must_use]
pub fn snapshot_slug(repo: &str) -> String {
    repo.to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The cached snapshot path for `repo` under `workspace_root`.
#[must_use]
pub fn snapshot_path(workspace_root: &Path, repo: &str) -> PathBuf {
    snapshot_dir(workspace_root).join(format!("fleet-{}.json", snapshot_slug(repo)))
}

/// Read the snapshot at `path`. `None` when it is absent, unreadable, or not
/// [`SNAPSHOT_SCHEMA`] — an unknown schema is a refusal, never a partial parse.
#[must_use]
pub fn read(path: &Path) -> Option<FleetSnapshot> {
    let text = std::fs::read_to_string(path).ok()?;
    let snapshot: FleetSnapshot = serde_json::from_str(&text).ok()?;
    (snapshot.schema == SNAPSHOT_SCHEMA).then_some(snapshot)
}

/// Write `snapshot` to `path` through a temp file and a rename, so a crash
/// mid-write cannot leave a half-written cache behind.
///
/// # Errors
///
/// The parent could not be created, or the write/rename failed.
pub fn write(path: &Path, snapshot: &FleetSnapshot) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(snapshot).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, format!("{text}\n"))?;
    std::fs::rename(&tmp, path)
}

/// Every cached snapshot under `workspace_root`, by ascending file name.
///
/// The daemon's read path: a plain directory listing and a few file reads, no
/// forge call, so putting fleet history on a tick costs nothing.
#[must_use]
pub fn load_all(workspace_root: &Path) -> Vec<FleetSnapshot> {
    let dir = snapshot_dir(workspace_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    paths.iter().filter_map(|p| read(p)).collect()
}

/// Every cached snapshot under `workspace_root` as one [`StageSamples`], or
/// `None` when there is no snapshot at all.
///
/// `None` rather than an empty fleet history on purpose: "no snapshot" and "a
/// snapshot with nothing in it" are different answers, and only the first
/// should fall back to the local view.
#[must_use]
pub fn load_history(workspace_root: &Path) -> Option<StageSamples> {
    let snapshots = load_all(workspace_root);
    if snapshots.is_empty() {
        return None;
    }
    let mut history = StageSamples {
        scope: HistoryScope::Fleet,
        ..StageSamples::default()
    };
    for snapshot in &snapshots {
        history.merge(snapshot.stage_samples());
    }
    Some(history)
}

/// The history an estimate reads under `mode`, given this host's already-built
/// `local` view — the single place the scope decision is made, so the daemon
/// and the CLI cannot disagree about what `fleet` means.
///
/// - [`HistoryScopeMode::Local`] — `local` unchanged (`scope = local`).
/// - [`HistoryScopeMode::Augment`] — `local` **plus** every cached snapshot
///   (`scope = fleet` once one exists, `local` when none does).
/// - [`HistoryScopeMode::Fleet`] — the snapshots alone, so the estimate is a
///   pure function of a named snapshot; `local` when there is no snapshot,
///   because a missing cache is not evidence of an empty history.
#[must_use]
pub fn apply_scope(
    mode: HistoryScopeMode,
    workspace_root: &Path,
    local: StageSamples,
) -> StageSamples {
    match mode {
        HistoryScopeMode::Local => local,
        HistoryScopeMode::Augment => match load_history(workspace_root) {
            Some(fleet) => {
                let mut merged = local;
                merged.merge(fleet);
                merged
            }
            None => local,
        },
        HistoryScopeMode::Fleet => load_history(workspace_root).unwrap_or(local),
    }
}
