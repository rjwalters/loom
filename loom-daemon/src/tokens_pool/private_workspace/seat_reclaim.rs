//! Reclaim inside private-workspace seat volumes (#14678, Asks 1 and 3 — the
//! periodic idle pass; the shared object store is #14680, the per-seat size
//! telemetry is #14693).
//!
//! # What this reclaims, and where it runs
//!
//! A seat volume (`/workspace` inside `loom-codex-workspace-<seat>`) grows as
//! seats × registered repos × (clone + worktrees + scratch) and the host-side
//! reaper never sees inside it — its `reaper.pass` spans report
//! `preserved.private_workspace` for everything there. The disk-pressure
//! incident behind this issue (two workers at 100%, 224 GB across 7 seats)
//! was mostly per-step scratch and finished-work checkouts no host pass could
//! reach. This module adds the missing in-volume pass:
//!
//! - **Scratch**: `cache/<role>-<n>.*` entries and top-level step dirs
//!   (`<role>-<n>.*`, `*-tmp`) older than the existing
//!   `autonomous.worktreeReaper.scratchReclaim.maxAgeHours`.
//! - **Worktrees**: linked checkouts under `repo` and `work/<slug>` that are
//!   clean, fully published, and whose branch has a confirmed **merged** PR
//!   at that exact tip past the existing reaper grace
//!   (`autonomous.worktreeReaper.gracePeriodSecs`).
//!
//! The pass runs in two halves. The container half ([`seat_reclaim`]) is a
//! `private-workspace seat-reclaim` verb driven like `prepare`/`recover`; it
//! never mutates `.identity` records, never quarantines and never takes a
//! backup — a worktree it cannot prove finished is preserved with a reason.
//! The host half ([`run_for`], called from the eager-reclaim tier 1 the way
//! [`crate::scratch_reclaim`] is) enumerates the seats, takes each account's
//! exclusive lease, requires the container running and idle — the same
//! admission [`super::lifecycle::run_job`] applies — and execs the verb. It
//! never calls `docker::prepare_target`: preparation runs `clean_worktrees`,
//! which refuses a dirty worktree, and a dirty seat must still get its
//! scratch reclaimed and its size reported.
//!
//! # Ownership vs per-job preparation (#14483)
//!
//! Job preparation (merged #14483) owns the **target clone at admission
//! time**: it forgives untracked caches, force-reaps linked worktrees whose
//! tip has a *finished* PR — merged **or closed-unmerged**, dirty ones
//! included — and offers the explicit `accounts session recover` listing.
//! This pass owns **all seats and all clones, idle**: it is merged-only,
//! clean-only, and publishes what it kept through `reaper.pass`. The dirty
//! sibling in a seat with a published tip at a merged PR is preparation's to
//! remove and this pass's to preserve; see
//! `defaults/docs/private-session-dispatch.md`.
//!
//! # Safety
//!
//! Liveness is structural, not probed: the verb only ever runs while the
//! seat's account lease is held and the container is proven idle, so no step
//! session can be live in the volume; the scratch age floor then bounds the
//! blast radius to entries no recent step wrote. Future-dated mtimes are
//! never reclaimable ([`crate::scratch_reclaim::is_reclaimable_age`]).

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::repository;
use crate::worktree_ops::clean::check_grace_period;
use crate::worktree_ops::clean_owner::PrAtHead;

// ============================================================================
// The verb (container side)
// ============================================================================

#[derive(clap::Args)]
pub struct SeatReclaimArgs {
    #[arg(long)]
    pub account: String,
    /// Scratch age floor in seconds; the host resolves it from
    /// `autonomous.worktreeReaper.scratchReclaim.maxAgeHours`.
    #[arg(long)]
    pub max_age_secs: i64,
    /// Post-merge grace in seconds; the host resolves it from
    /// `autonomous.worktreeReaper.gracePeriodSecs`.
    #[arg(long)]
    pub grace_secs: i64,
}

/// One pass's outcome, reported to the host as the protocol `outcome`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatReclaimOutcome {
    /// Linked worktrees evaluated (the main checkout is never a candidate).
    pub worktrees_scanned: usize,
    pub worktrees_removed: usize,
    pub worktrees_failed: usize,
    /// The preserve wording of every worktree left in place.
    pub preserved: Vec<String>,
    /// Bytes freed by worktree removals plus scratch removals.
    pub bytes: u64,
    /// Step-scratch entries removed (cache and top-level).
    pub scratch_removed: usize,
    /// Step-scratch entries evaluated (shape-matched candidates).
    pub scratch_scanned: usize,
    /// Bytes freed by scratch removals alone.
    pub scratch_bytes: u64,
}

/// The PR-state lookup the worktree half consults, injected so tests never
/// contact a forge: `(clone, repository, branch, tip) -> state`.
pub type PrLookup<'a> = dyn Fn(&Path, &str, &str, &str) -> PrAtHead + 'a;

/// One in-volume pass. `root` is the seat volume root (`/workspace`).
pub(super) fn seat_reclaim(
    root: &Path,
    args: &SeatReclaimArgs,
    pr_at_head: &PrLookup<'_>,
    now: DateTime<Utc>,
) -> anyhow::Result<SeatReclaimOutcome> {
    if args.account.is_empty() {
        anyhow::bail!("seat reclaim requires the account identity");
    }
    let root = root.canonicalize()?;
    let mut outcome = SeatReclaimOutcome::default();
    reclaim_scratch(&root, args.max_age_secs, now, &mut outcome);
    reap_merged_worktrees(&root, args, pr_at_head, now, &mut outcome)?;
    Ok(outcome)
}

// ============================================================================
// Scratch selection (pure gates + the sweep)
// ============================================================================

/// Whether `name` matches the per-step scratch shape `<role>-<n>` with an
/// optional `.`/`-` remainder: `sta-6421.tar.gz`, `judge-9661`,
/// `builder-6940.x`, `squad-6940-dir`, `loom-7456-check.log`. Reserved roots
/// (`repo`, `work`, `cache`, `identity.json`, dot-entries) match no prefix of
/// this shape. Pure.
#[must_use]
pub fn is_step_scratch_name(name: &str) -> bool {
    let Some((role, rest)) = name.split_once('-') else {
        return false;
    };
    if role.is_empty() || !role.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    let remainder = &rest[digits..];
    remainder.is_empty() || remainder.starts_with('.') || remainder.starts_with('-')
}

/// Whether `name` is a top-level step dir ending in `-tmp`:
/// `doctor-6422-tmp`, `curator-repro-tmp`. Pure.
#[must_use]
pub fn is_tmp_step_name(name: &str) -> bool {
    name.len() > 4 && name.ends_with("-tmp")
}

fn reclaim_scratch(
    root: &Path,
    max_age_secs: i64,
    now: DateTime<Utc>,
    outcome: &mut SeatReclaimOutcome,
) {
    let max_age_secs = max_age_secs.max(0);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join("cache")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_step_scratch_name(&name) {
                candidates.push(entry.path());
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_step_scratch_name(&name) || is_tmp_step_name(&name) {
                candidates.push(entry.path());
            }
        }
    }
    for path in candidates {
        outcome.scratch_scanned += 1;
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let modified: DateTime<Utc> = modified.into();
        let age_secs = (now - modified).num_seconds();
        if !crate::scratch_reclaim::is_reclaimable_age(age_secs, max_age_secs) {
            continue;
        }
        let bytes = if path.is_dir() {
            tree_bytes(&path)
        } else {
            metadata.len()
        };
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if removed.is_ok() {
            outcome.scratch_removed += 1;
            outcome.scratch_bytes += bytes;
            outcome.bytes += bytes;
        } else {
            log::debug!("seat_reclaim: could not remove {}: {:?}", path.display(), removed);
        }
    }
}

fn tree_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    total += tree_bytes(&entry.path());
                } else if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            }
        }
    }
    total
}

// ============================================================================
// Worktree reap (merged-only, clean-only, in-volume)
// ============================================================================

/// Every clone of the volume with its recorded repository: the legacy primary
/// (`repo`, identity at the volume root) and each `work/<slug>` carrying a
/// `work/.identity/<slug>.json` record. A staging dir (`.cloning-*`) and an
/// unrecorded directory are not ours to touch and are not enumerated.
fn volume_clones(root: &Path) -> Vec<(PathBuf, String)> {
    let mut clones = Vec::new();
    let primary_record = root.join("identity.json");
    let primary = root.join("repo");
    if primary.is_dir() {
        if let Ok(Some((repository, _))) = repository::recorded_identity(&primary_record) {
            clones.push((primary, repository));
        }
    }
    let work = root.join("work");
    let identities = work.join(".identity");
    if let Ok(entries) = std::fs::read_dir(&work) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !entry.path().is_dir() || !super::safe_segment(name) || name.contains("..") {
                continue;
            }
            let record = identities.join(format!("{name}.json"));
            if let Ok(Some((repository, _))) = repository::recorded_identity(&record) {
                clones.push((entry.path(), repository));
            }
        }
    }
    clones
}

fn reap_merged_worktrees(
    root: &Path,
    args: &SeatReclaimArgs,
    pr_at_head: &PrLookup<'_>,
    now: DateTime<Utc>,
    outcome: &mut SeatReclaimOutcome,
) -> anyhow::Result<()> {
    for (repo, repository) in volume_clones(root) {
        for entry in repository::worktree_entries(&repo)? {
            let path = &entry.path;
            outcome.worktrees_scanned += 1;
            let Some(canonical) = path.canonicalize().ok() else {
                outcome.preserved.push("worktree state unreadable".into());
                continue;
            };
            if canonical == repo {
                // The main checkout is what the seat IS; never a candidate.
                outcome.worktrees_scanned -= 1;
                continue;
            }
            if entry.locked {
                outcome.preserved.push("worktree locked (in use)".into());
                continue;
            }
            if !canonical.starts_with(root) {
                outcome
                    .preserved
                    .push("worktree outside the private volume".into());
                continue;
            }
            let branch = repository::git(path, &["rev-parse", "--abbrev-ref", "HEAD"])?
                .trim()
                .to_owned();
            if branch == "HEAD" {
                outcome
                    .preserved
                    .push("detached HEAD (unnamed work)".into());
                continue;
            }
            if repository::ahead_of_origin(path, "HEAD")? > 0 {
                outcome.preserved.push("unpushed commits on HEAD".into());
                continue;
            }
            if !repository::worktree_status(path)?.is_empty() {
                outcome
                    .preserved
                    .push("uncommitted changes in worktree".into());
                continue;
            }
            let tip = repository::git(path, &["rev-parse", "HEAD"])?
                .trim()
                .to_owned();
            match pr_at_head(&repo, &repository, &branch, &tip) {
                PrAtHead::Open => outcome.preserved.push("PR still open".into()),
                PrAtHead::ClosedNoMerge => outcome
                    .preserved
                    .push("PR closed without merging (not merged)".into()),
                PrAtHead::NoPr => outcome
                    .preserved
                    .push("no merged PR for the branch at this tip".into()),
                PrAtHead::Unknown => outcome.preserved.push("PR status unknown".into()),
                PrAtHead::Merged(merged_at) => {
                    let Ok(merged) = DateTime::parse_from_rfc3339(&merged_at) else {
                        outcome
                            .preserved
                            .push("PR status unknown (unparseable merge time)".into());
                        continue;
                    };
                    let (past, _) =
                        check_grace_period(merged.with_timezone(&Utc), args.grace_secs, now);
                    if !past {
                        outcome
                            .preserved
                            .push("merged PR grace period not passed".into());
                        continue;
                    }
                    let held = tree_bytes(path);
                    if repository::git(&repo, &["worktree", "remove", &path.display().to_string()])
                        .is_ok()
                    {
                        outcome.worktrees_removed += 1;
                        outcome.bytes += held;
                    } else {
                        outcome.worktrees_failed += 1;
                    }
                }
            }
        }
    }
    Ok(())
}

/// The production lookup the verb binds: resolves the GitHub host/slug of
/// the clone's own repository URL, then asks
/// [`crate::worktree_ops::clean_owner::pr_at_head`] with the clone as the
/// probe's working directory. A repository on any other forge answers
/// [`PrAtHead::Unknown`] — preserved, never removed on a guess.
pub(super) fn github_pr_at_head(
    repo: &Path,
    repository: &str,
    branch: &str,
    tip: &str,
) -> PrAtHead {
    let host = std::env::var("GH_HOST").unwrap_or_else(|_| "github.com".into());
    let Some(slug) = super::forge_auth::github_slug(repository, &host) else {
        return PrAtHead::Unknown;
    };
    crate::worktree_ops::clean_owner::pr_at_head(repo, &host, &slug, branch, tip)
}

// ============================================================================
// The host-side pass (leased, idle-only, no preparation)
// ============================================================================

/// One host-wide pass's outcome over every private-workspace seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatReclaimReport {
    /// `Some(reason)` when no pass ran at all (disabled, or the cooldown had
    /// not elapsed). `None` on a pass that ran.
    pub skipped: Option<String>,
    /// Per-seat results: the account name and either the verb's outcome or
    /// the reason this seat was skipped/refused (busy, stopped, not idle, a
    /// containment-validation refusal, an endpoint error).
    pub seats: Vec<(String, Result<SeatReclaimOutcome, String>)>,
}

impl SeatReclaimReport {
    /// Worktrees removed across every seat that ran.
    #[must_use]
    pub fn worktrees_removed(&self) -> usize {
        self.seats
            .iter()
            .filter_map(|(_, result)| result.as_ref().ok())
            .map(|outcome| outcome.worktrees_removed)
            .sum()
    }

    /// Scratch entries removed across every seat that ran.
    #[must_use]
    pub fn scratch_removed(&self) -> usize {
        self.seats
            .iter()
            .filter_map(|(_, result)| result.as_ref().ok())
            .map(|outcome| outcome.scratch_removed)
            .sum()
    }

    /// Bytes freed across every seat that ran.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.seats
            .iter()
            .filter_map(|(_, result)| result.as_ref().ok())
            .map(|outcome| outcome.bytes)
            .sum()
    }

    /// One human-readable summary for the eager-reclaim log line.
    #[must_use]
    pub fn summary(&self) -> String {
        if let Some(reason) = &self.skipped {
            return format!("skipped: {reason}");
        }
        format!(
            "{} seat(s): {} worktree(s), {} scratch entries ({})",
            self.seats.len(),
            self.worktrees_removed(),
            self.scratch_removed(),
            crate::fs_util::human_size(self.bytes())
        )
    }
}

/// Host-wide cooldown state: one marker key, not per-repo — the pass walks
/// every seat from whatever root the eager trigger names, so a per-root key
/// would re-run it once per registered root per tick.
static LAST_RUN_AT: std::sync::OnceLock<std::sync::Mutex<Option<DateTime<Utc>>>> =
    std::sync::OnceLock::new();

fn last_run_slot() -> &'static std::sync::Mutex<Option<DateTime<Utc>>> {
    LAST_RUN_AT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Drop cooldown state. Unused today (the host driver path needs a full
/// account registry fixture); kept as the standard seam for the first test
/// that gets one.
#[cfg(test)]
#[allow(dead_code)]
#[doc(hidden)]
pub fn reset_state_for_test() {
    *last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

/// Whether the host-wide cooldown suppresses a pass at `now`. Pure: the
/// decision the report's `skipped` reason is built from.
#[must_use]
pub fn cooldown_skip(
    last: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    min_interval_secs: u64,
) -> Option<String> {
    let last = last?;
    let since = (now - last).num_seconds();
    (since >= 0 && since < i64::try_from(min_interval_secs).unwrap_or(i64::MAX))
        .then(|| format!("a pass ran {since}s ago (cooldown {min_interval_secs}s)"))
}

/// One production pass over every private-workspace seat (#14678), honoring
/// the existing `autonomous.worktreeReaper.scratchReclaim.*` surface for its
/// enable gate, scratch age floor and cooldown — no new flags. The worktree
/// half's grace comes from the existing
/// `autonomous.worktreeReaper.gracePeriodSecs`. Scheduled from the eager
/// reclaim tier 1 like [`crate::scratch_reclaim::run_for`].
pub fn run_for(workspace: &Path) -> SeatReclaimReport {
    use crate::observability::ops::reaper;

    let config = crate::scratch_reclaim::read_scratch_reclaim_config(workspace);
    let now = Utc::now();
    if !crate::scratch_reclaim::resolve_enabled(&config) {
        let report = SeatReclaimReport {
            skipped: Some("disabled (scratchReclaim.enabled=false)".into()),
            seats: Vec::new(),
        };
        log::debug!("private_workspace seat_reclaim: {}", report.summary());
        return report;
    }
    let min_interval_secs = crate::scratch_reclaim::resolve_min_interval_secs(&config);
    let last = *last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(reason) = cooldown_skip(last, now, min_interval_secs) {
        let report = SeatReclaimReport {
            skipped: Some(reason),
            seats: Vec::new(),
        };
        log::debug!("private_workspace seat_reclaim: {}", report.summary());
        return report;
    }
    *last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(now);

    let max_age_secs =
        i64::try_from(crate::scratch_reclaim::resolve_max_age_hours(&config).saturating_mul(3600))
            .unwrap_or(i64::MAX);
    let grace_secs = crate::worktree_reaper::resolve_grace_period(
        &crate::worktree_reaper::read_worktree_reaper_config(workspace),
    );
    let seats = super::lifecycle::seat_reclaim(workspace, max_age_secs, grace_secs);

    for (account, result) in &seats {
        let Ok(outcome) = result else {
            log::debug!("private_workspace seat_reclaim: {account} skipped: {result:?}");
            continue;
        };
        // The span's repo label is the seat's account name: one path-shaped
        // carrier, never a host path (`reaper::pass_span` takes the final
        // component only).
        let seat = Path::new("/private-workspace-seats").join(account);
        let worktree_bytes = outcome.bytes.saturating_sub(outcome.scratch_bytes);
        let preserved: Vec<&str> = outcome.preserved.iter().map(String::as_str).collect();
        reaper::publish(
            &seat,
            &reaper::Pass {
                class: "seat",
                phase: reaper::PHASE_REAP,
                scanned: outcome.worktrees_scanned,
                removed: outcome.worktrees_removed,
                failed: outcome.worktrees_failed,
                preserved: preserved.clone(),
                bytes: worktree_bytes,
                backed_up: 0,
                backup_refs: Vec::new(),
            },
        );
        reaper::publish(
            &seat,
            &reaper::Pass {
                class: "seat",
                phase: reaper::PHASE_RECLAIM,
                scanned: outcome.scratch_scanned,
                removed: outcome.scratch_removed,
                failed: 0,
                preserved: Vec::new(),
                bytes: outcome.scratch_bytes,
                backed_up: 0,
                backup_refs: Vec::new(),
            },
        );
        if outcome.worktrees_removed > 0 || outcome.scratch_removed > 0 {
            log::info!(
                "private_workspace seat_reclaim: seat {account} removed {} worktree(s) and {} \
                 scratch entries ({}) — preserved {preserved:?}",
                outcome.worktrees_removed,
                outcome.scratch_removed,
                crate::fs_util::human_size(outcome.bytes),
            );
        } else {
            log::debug!(
                "private_workspace seat_reclaim: seat {account} nothing to reclaim — preserved \
                 {preserved:?}"
            );
        }
    }

    let report = SeatReclaimReport {
        skipped: None,
        seats,
    };
    if report.worktrees_removed() > 0 || report.scratch_removed() > 0 {
        log::info!(
            "private_workspace seat_reclaim: {} reclaimed across {} seat(s)",
            report.summary(),
            report.seats.len()
        );
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_git_helpers::{configure, run as raw_git};
    use crate::worktree_ops::clean_owner::pr_at_head_in_rows;
    use chrono::Duration;

    /// The volume fixture: a remote, a prepared primary clone (`repo` with
    /// its identity record), and nothing else — the same shape
    /// [`super::super::tests::RepoFixture`] builds for the prepare tests.
    struct Volume {
        _temp: tempfile::TempDir,
        remote: PathBuf,
        root: PathBuf,
    }

    impl Volume {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let remote = temp.path().join("remote");
            let root = temp.path().join("volume");
            std::fs::create_dir(&remote).unwrap();
            std::fs::create_dir(&root).unwrap();
            raw_git(&remote, &["init", "-b", "main"]);
            configure(&remote);
            std::fs::write(remote.join("fixture"), "base").unwrap();
            raw_git(&remote, &["add", "."]);
            raw_git(&remote, &["commit", "-m", "fixture"]);
            let args = repository::PrepareArgs {
                account: "fixture".into(),
                repository: remote.to_string_lossy().into_owned(),
                base: "main".into(),
                container_id: "test-container".into(),
                branch: None,
                slug: None,
            };
            repository::prepare(&root, &args).unwrap();
            Self {
                _temp: temp,
                remote,
                root,
            }
        }

        fn repo(&self) -> PathBuf {
            self.root.join("repo")
        }

        /// A linked worktree under the clone's `.loom/worktrees`, on a new
        /// branch with one commit; `push` publishes the branch and fetches it
        /// back, so the clone's remote-tracking refs name the tip (the
        /// unpublished-branch gate reads those, not the remote).
        fn worktree(&self, name: &str, push: bool) -> PathBuf {
            let branch = format!("feature/{name}");
            let path = self.repo().join(".loom/worktrees").join(name);
            raw_git(
                &self.repo(),
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    path.to_string_lossy().as_ref(),
                ],
            );
            std::fs::write(path.join(name), "work").unwrap();
            raw_git(&path, &["add", "."]);
            raw_git(&path, &["commit", "-m", name]);
            if push {
                raw_git(&path, &["push", self.remote.to_string_lossy().as_ref(), &branch]);
                // Update the clone's remote-tracking refs: `rev-list --not
                // --remotes=origin` reads those, not the remote itself.
                raw_git(&path, &["fetch", "origin"]);
            }
            path
        }
    }

    fn args(max_age_secs: i64, grace_secs: i64) -> SeatReclaimArgs {
        SeatReclaimArgs {
            account: "fixture".into(),
            max_age_secs,
            grace_secs,
        }
    }

    fn merged_days_ago(days: i64) -> String {
        (Utc::now() - Duration::days(days)).to_rfc3339()
    }

    // ===================================================================
    // Pure gates
    // ===================================================================

    #[test]
    fn step_scratch_shapes_match_only_step_names() {
        for name in [
            "sta-6421",
            "sta-6421.tar.gz",
            "judge-9661.json",
            "builder-6940.x",
            "squad-6940-dir",
            "loom-7456-check.log",
            "role_1-2",
        ] {
            assert!(is_step_scratch_name(name), "{name}");
        }
        for reserved in [
            "repo",
            "work",
            "cache",
            "identity.json",
            ".identity",
            ".cloning-x",
            "main",
            "not-step",
            "steps-late",
            "builder-x",
            "-6422",
        ] {
            assert!(!is_step_scratch_name(reserved), "{reserved}");
        }
        assert!(is_tmp_step_name("doctor-6422-tmp"));
        assert!(is_tmp_step_name("curator-repro-tmp"));
        assert!(!is_tmp_step_name("tmp"));
        assert!(!is_tmp_step_name("work"));
    }

    // ===================================================================
    // The periodic-pass fixture: eligible beside dirty / unpublished /
    // open-PR / closed-unmerged siblings. Only the eligible checkout
    // disappears (#14678).
    // ===================================================================

    #[test]
    fn only_the_eligible_checkout_is_removed() {
        let _fork = super::super::tests::fork_guard();
        let volume = Volume::new();
        // Everything a removal needs — so for the dirty sibling, published
        // tip at a merged PR past grace, cleanliness alone preserves it.
        let eligible = volume.worktree("issue-10", true);
        let dirty = volume.worktree("issue-11", true);
        std::fs::write(dirty.join("untracked"), "dirty").unwrap();
        let unpublished = volume.worktree("issue-12", false);
        let open = volume.worktree("issue-13", true);
        let closed_unmerged = volume.worktree("issue-14", true);
        let within_grace = volume.worktree("issue-15", true);

        let lookup = |_: &Path, _: &str, branch: &str, _: &str| -> PrAtHead {
            match branch {
                "feature/issue-10" | "feature/issue-11" => PrAtHead::Merged(merged_days_ago(30)),
                "feature/issue-13" => PrAtHead::Open,
                "feature/issue-14" => PrAtHead::ClosedNoMerge,
                "feature/issue-15" => PrAtHead::Merged(merged_days_ago(0)),
                _ => PrAtHead::NoPr,
            }
        };
        let outcome = seat_reclaim(&volume.root, &args(0, 3600), &lookup, Utc::now()).unwrap();

        assert!(!eligible.exists(), "the eligible checkout must disappear");
        assert!(dirty.exists(), "dirty is preserved by cleanliness alone");
        assert!(unpublished.exists(), "unpublished is preserved");
        assert!(open.exists(), "an open PR is preserved");
        assert!(closed_unmerged.exists(), "closed-unmerged is preserved");
        assert!(within_grace.exists(), "merged inside the grace window is preserved");

        assert_eq!(outcome.worktrees_removed, 1);
        assert_eq!(outcome.worktrees_failed, 0);
        assert_eq!(outcome.worktrees_scanned, 6);
        let reasons = outcome.preserved.join("\n");
        assert!(reasons.contains("uncommitted changes in worktree"), "{reasons}");
        assert!(reasons.contains("unpushed commits on HEAD"), "{reasons}");
        assert!(reasons.contains("PR still open"), "{reasons}");
        assert!(reasons.contains("PR closed without merging"), "{reasons}");
        assert!(reasons.contains("merged PR grace period not passed"), "{reasons}");
        // The branch ref and its commits survive the checkout's removal.
        assert!(
            raw_git(&volume.repo(), &["rev-parse", "--verify", "refs/heads/feature/issue-10"])
                .len()
                == 40
        );
    }

    #[test]
    fn main_checkout_and_foreign_worktrees_are_never_candidates() {
        let _fork = super::super::tests::fork_guard();
        let volume = Volume::new();
        // A worktree the clone knows about but that lives outside the volume
        // (a host path): preserved, never followed.
        let outside = volume._temp.path().join("outside-wt");
        raw_git(
            &volume.repo(),
            &[
                "worktree",
                "add",
                "-b",
                "feature/issue-20",
                outside.to_string_lossy().as_ref(),
            ],
        );
        raw_git(
            &outside,
            &[
                "push",
                volume.remote.to_string_lossy().as_ref(),
                "feature/issue-20",
            ],
        );
        raw_git(&outside, &["fetch", "origin"]);
        let lookup = |_: &Path, _: &str, branch: &str, _: &str| -> PrAtHead {
            if branch == "feature/issue-20" {
                PrAtHead::Merged(merged_days_ago(30))
            } else {
                PrAtHead::NoPr
            }
        };
        let outcome = seat_reclaim(&volume.root, &args(0, 0), &lookup, Utc::now()).unwrap();
        assert_eq!(outcome.worktrees_removed, 0);
        assert!(outside.exists());
        assert!(outcome
            .preserved
            .iter()
            .any(|r| r.contains("outside the private volume")));
    }

    // ===================================================================
    // Scratch: ended vs live step, future-dated mtime, reserved roots
    // ===================================================================

    #[test]
    fn scratch_reclaim_removes_ended_steps_and_keeps_live_and_future_dated_ones() {
        let _fork = super::super::tests::fork_guard();
        let volume = Volume::new();
        let cache = volume.root.join("cache");
        std::fs::create_dir_all(cache.join("sta-6421.tar")).unwrap();
        std::fs::write(cache.join("sta-6421.tar").join("blob"), "4 GB").unwrap();
        std::fs::write(cache.join("judge-9661.json"), "log").unwrap();
        std::fs::write(cache.join("tool-cache"), "not a step").unwrap();
        std::fs::create_dir_all(volume.root.join("doctor-6422-tmp")).unwrap();
        std::fs::create_dir_all(volume.root.join("builder-6940.x")).unwrap();
        // A live step's scratch and a clock-skewed one: never reclaimable.
        std::fs::write(cache.join("live-7777.tar"), "fresh").unwrap();
        std::fs::write(cache.join("skewed-8888.tar"), "future").unwrap();

        let now = Utc::now();
        // Ended steps: mtime 48h in the past.
        for path in [
            cache.join("sta-6421.tar"),
            cache.join("judge-9661.json"),
            volume.root.join("doctor-6422-tmp"),
            volume.root.join("builder-6940.x"),
            cache.join("tool-cache"),
        ] {
            set_mtime(&path, now - Duration::hours(48));
        }
        // Clock skew: an mtime in the future.
        set_mtime(&cache.join("skewed-8888.tar"), now + Duration::hours(2));

        let outcome =
            seat_reclaim(&volume.root, &args(24 * 3600, 3600), &|_, _, _, _| PrAtHead::NoPr, now)
                .unwrap();
        assert_eq!(outcome.scratch_removed, 4, "{:?}", outcome);
        assert_eq!(outcome.scratch_scanned, 6);
        assert!(!cache.join("sta-6421.tar").exists());
        assert!(!cache.join("judge-9661.json").exists());
        assert!(!volume.root.join("doctor-6422-tmp").exists());
        assert!(!volume.root.join("builder-6940.x").exists());
        assert!(cache.join("tool-cache").exists(), "non-step cache entries stay");
        assert!(cache.join("live-7777.tar").exists(), "a live step's scratch stays");
        assert!(cache.join("skewed-8888.tar").exists(), "a future-dated entry stays");
        assert!(outcome.scratch_bytes > 0);
        // The reserved roots are never candidates at any age.
        assert!(volume.root.join("repo").exists());
        assert!(volume.root.join("cache").exists());
        assert!(volume.root.join("identity.json").exists());
    }

    /// Stamp an mtime (files and directories) the way the age gates read it.
    fn set_mtime(path: &Path, when: DateTime<Utc>) {
        let out = std::process::Command::new("touch")
            .arg("-d")
            .arg(when.to_rfc3339())
            .arg(path)
            .output()
            .expect("touch must spawn");
        assert!(
            out.status.success(),
            "touch {} failed: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn a_dirty_seat_still_reclaims_its_scratch() {
        let _fork = super::super::tests::fork_guard();
        let volume = Volume::new();
        std::fs::write(volume.repo().join("stray"), "unfinished work").unwrap();
        let cache = volume.root.join("cache");
        std::fs::write(cache.join("sta-6421.tar"), "4 GB").unwrap();

        let outcome = seat_reclaim(
            &volume.root,
            &args(0, 3600),
            &|_, _, _, _| PrAtHead::Merged(merged_days_ago(30)),
            Utc::now(),
        )
        .unwrap();
        assert!(!cache.join("sta-6421.tar").exists(), "scratch is reclaimed");
        assert_eq!(outcome.scratch_removed, 1);
        assert_eq!(outcome.worktrees_removed, 0);
        assert!(volume.repo().join("stray").exists(), "the dirty main checkout is untouched");
    }

    // ===================================================================
    // The merged-only row classification the verb's lookup builds on
    // (#14678: closed-unmerged is preserved here, finished-at-head is not
    // the same question).
    // ===================================================================

    fn row(
        state: &str,
        merged_at: Option<&str>,
        closed_at: Option<&str>,
        sha: &str,
    ) -> crate::worktree_ops::clean_owner::PrRowRest {
        serde_json::from_value(serde_json::json!({
            "state": state,
            "merged_at": merged_at,
            "closed_at": closed_at,
            "head": {"ref": "feature/x", "sha": sha, "repo": {"full_name": "Owner/Repo"}},
            "base": {"repo": {"full_name": "Owner/Repo"}},
        }))
        .unwrap()
    }

    /// The one shape every eligible sibling is built from: closed and merged
    /// at `sha`.
    fn merged(sha: &str) -> crate::worktree_ops::clean_owner::PrRowRest {
        row("closed", Some("2020-01-01T00:00:00Z"), Some("2020-01-01T00:00:00Z"), sha)
    }

    #[test]
    fn pr_at_head_rows_are_merged_only_and_open_wins() {
        assert_eq!(
            pr_at_head_in_rows(&[merged("tip")], "Owner/Repo", "feature/x", "tip"),
            PrAtHead::Merged("2020-01-01T00:00:00Z".into())
        );
        // A newer open PR on the same branch takes precedence over the merge.
        let open = row("open", None, None, "other");
        assert_eq!(
            pr_at_head_in_rows(&[merged("tip"), open], "Owner/Repo", "feature/x", "tip"),
            PrAtHead::Open
        );
        // Closed without merging at the tip: preserved by the seat pass.
        let closed = row("closed", None, Some("2020-01-01T00:00:00Z"), "tip");
        assert_eq!(
            pr_at_head_in_rows(&[closed], "Owner/Repo", "feature/x", "tip"),
            PrAtHead::ClosedNoMerge
        );
        // A merge at a different tip leaves no merge at this tip.
        assert_eq!(
            pr_at_head_in_rows(&[merged("older")], "Owner/Repo", "feature/x", "tip"),
            PrAtHead::NoPr
        );
        // A truncated or foreign record is never evidence.
        assert_eq!(
            pr_at_head_in_rows(&[merged("tip")], "Other/Repo", "feature/x", "tip"),
            PrAtHead::Unknown
        );
        let many: Vec<_> = (0..100).map(|i| merged(&format!("tip{i}"))).collect();
        assert_eq!(pr_at_head_in_rows(&many, "Owner/Repo", "feature/x", "tip"), PrAtHead::Unknown);
    }

    // ===================================================================
    // The host report folds seat outcomes
    // ===================================================================

    #[test]
    fn the_host_wide_cooldown_suppresses_only_a_recent_pass() {
        let now = Utc::now();
        assert!(cooldown_skip(None, now, 1800).is_none(), "a first pass runs");
        assert!(
            cooldown_skip(Some(now - chrono::Duration::seconds(60)), now, 1800).is_some(),
            "a pass 60s ago is inside a 30m cooldown"
        );
        let held = cooldown_skip(Some(now - chrono::Duration::seconds(1800)), now, 1800);
        assert!(held.is_none(), "a pass exactly one cooldown old re-runs: {held:?}");
        // Clock skew backwards never wedges the cooldown shut.
        assert!(cooldown_skip(Some(now + chrono::Duration::hours(1)), now, 1800).is_none());
    }

    #[test]
    fn report_sums_across_seats() {
        let one = SeatReclaimOutcome {
            worktrees_scanned: 3,
            worktrees_removed: 1,
            worktrees_failed: 0,
            preserved: vec!["PR still open".into()],
            bytes: 100,
            scratch_removed: 2,
            scratch_scanned: 2,
            scratch_bytes: 40,
        };
        let report = SeatReclaimReport {
            skipped: None,
            seats: vec![
                ("agent-1".into(), Ok(one)),
                ("agent-2".into(), Err("seat busy".into())),
            ],
        };
        assert_eq!(report.worktrees_removed(), 1);
        assert_eq!(report.scratch_removed(), 2);
        assert_eq!(report.bytes(), 100);
        assert!(report.summary().contains("worktree"));
        let skipped = SeatReclaimReport {
            skipped: Some("disabled".into()),
            seats: Vec::new(),
        };
        assert!(skipped.summary().contains("disabled"));
    }
}
