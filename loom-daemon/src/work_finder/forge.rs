//! Concrete [`WorkSource`] / [`WorkDispatcher`] implementations that wire the
//! finder to the live forge (`gh`) and the daemon's [`SweepRegistry`].
//!
//! The pure [`super::tick`] logic is exercised in tests via mocks; these
//! adapters are the runtime glue and shell out to `gh` / spawn children, so
//! they are not unit-tested directly (mirroring
//! [`crate::epic_supervisor::forge`]).
//!
//! Extracted from the inline `pub mod forge { … }` block in `work_finder.rs`
//! into its own file (#8572) for the same file-size-ratchet reason as
//! [`super::registry_refresh`]: `work_finder.rs` is over the threshold
//! (`.loom/docs/file-size-policy.md`) and may not grow. Pure move — the
//! module's position in the module tree, and therefore every `super::` path
//! inside it, is unchanged.

use super::{
    read_work_finder_config, resolve_extra_skip_labels_with_config, WorkDispatcher, WorkItem,
    WorkSource,
};
use crate::sweep_registry::SweepRegistry;
use crate::types::{SweepKind, SweepState};
use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A forge-backed [`WorkSource`] that lists open `loom:issue` items via
/// `gh`. Mirrors [`crate::epic_supervisor::forge::GhEpicSource`].
pub struct GhWorkSource {
    gh_bin: PathBuf,
    repo: Option<String>,
    /// Working directory the `gh` query runs in. When set (multi-workspace
    /// fan-out, #3928) `gh` auto-detects the repo from that root's git
    /// remote, so each registered workspace is polled against its own repo
    /// without a single machine-global `LOOM_REPO`. `None` keeps today's
    /// behavior (inherit the daemon's cwd).
    cwd: Option<PathBuf>,
    /// Whether the last [`WorkSource::list_ready_issues`] call read every
    /// listing whole (#11139); see [`WorkSource::listing_complete`].
    complete: bool,
}

impl GhWorkSource {
    /// Construct a source using `gh` from `PATH`, honoring `LOOM_REPO` for
    /// the `--repo` flag when set.
    #[must_use]
    pub fn new() -> Self {
        Self {
            gh_bin: PathBuf::from(crate::gh_invocation::gh_bin()),
            repo: std::env::var("LOOM_REPO").ok(),
            cwd: None,
            complete: true,
        }
    }

    /// Construct a source scoped to a specific workspace `root` (#3928): the
    /// `gh` query runs with `current_dir(root)` so it targets that repo's own
    /// remote. `LOOM_REPO`, when set, is still honored as a machine-global
    /// `--repo` override (preserving the single-workspace behavior
    /// byte-for-byte); in a genuine multi-repo deployment it is left unset so
    /// each root's cwd selects its repo.
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        Self {
            gh_bin: PathBuf::from(crate::gh_invocation::gh_bin()),
            repo: std::env::var("LOOM_REPO").ok(),
            cwd: Some(root.to_path_buf()),
            complete: true,
        }
    }

    /// Override the `gh` binary path (for tests / non-standard installs).
    #[must_use]
    pub fn with_gh_bin(mut self, bin: PathBuf) -> Self {
        self.gh_bin = bin;
        self
    }
}

impl Default for GhWorkSource {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkSource for GhWorkSource {
    fn list_ready_issues(&mut self) -> Result<Vec<WorkItem>> {
        // Curator intake reconcile (#10041): cadence-gated, fail-soft, REST-only;
        // gives every unlabeled issue `loom:triage` so Curator has one queue.
        // A label-transition write, so it runs on every tick, outside the gate.
        if let Some(root) = self.cwd.as_deref() {
            crate::intake_reconcile::maybe_run(&self.gh_bin, root);
            // The `loom:blocked` release pass (#10556) has its own task since
            // #10763: `crate::stale_blocked::release_task`.
        }
        // Event-gated polling (#9255, opt-in `forgeEvents.pollGating`, default
        // off): while the feed is `healthy`, a repo with no event since its
        // last poll returns its held *candidate* listing. Off, this is a bare
        // call. The claim itself (`dispatch`) re-reads the forge and is never
        // gated.
        let repo = self.repo.clone();
        let cwd = self.cwd.clone();
        let listed =
            crate::forge_events::poll_gate::gated_list(cwd.as_deref(), repo.as_deref(), || {
                self.list_ready_issues_uncached()
            });
        // #11139: a fresh listing that fell short is never held, so a gated
        // tick cannot serve partial rows as the whole queue. (A held listing
        // leaves `complete` at its per-tick default, `true`.)
        if !self.complete {
            crate::forge_events::poll_gate::forget_held(cwd.as_deref());
        }
        listed
    }

    fn listing_complete(&self) -> bool {
        self.complete
    }

    /// The open `loom:very-important` issues, any state (#11509): one more
    /// ETag-cached listing, read only on a tick whose ready listing holds a
    /// very-important issue (a 304 costs no rate limit). A failure is logged
    /// and returns `None` (an incomplete walk returns the rows read, which
    /// the cap unions with the ready listing); it never marks the ready queue incomplete, because
    /// the ready rows themselves were read whole.
    fn list_open_very_important(&mut self) -> Option<Vec<WorkItem>> {
        let (gh, cwd, repo) = (&self.gh_bin, self.cwd.as_deref(), self.repo.as_deref());
        let label = crate::priority_pick::VERY_IMPORTANT_LABEL;
        // 2am#4367 P5: a discovery listing, so the mirror may serve it.
        if let Some(rows) = mirror_rows(cwd, repo, label, true) {
            return Some(
                rows.into_iter()
                    .filter(|r| !r.is_pull_request)
                    .map(|r| WorkItem::with_created_at(r.number, r.labels, r.created_at))
                    .collect(),
            );
        }
        match crate::forge_listing::list_issues_cached_paged_as(
            "work_finder",
            gh,
            cwd,
            repo,
            label,
            "open",
        ) {
            Ok(listing) => {
                if let Some(e) = &listing.incomplete {
                    log::warn!(
                        "work_finder: the {label} listing is incomplete ({e:#}); the flood cap \
                         counts the {} rows read plus the ready listing",
                        listing.rows.len()
                    );
                }
                Some(
                    listing
                        .rows
                        .into_iter()
                        .filter(|r| !r.is_pull_request)
                        .map(|r| WorkItem::with_created_at(r.number, r.labels, r.created_at))
                        .collect(),
                )
            }
            Err(e) => {
                log::warn!(
                    "work_finder: listing {label} issues failed ({e}); the flood cap counts the \
                     ready listing only this tick"
                );
                crate::rate_limit_breaker::global_observe_failure(&e.to_string(), "work_finder");
                None
            }
        }
    }
}

impl GhWorkSource {
    fn list_ready_issues_uncached(&mut self) -> Result<Vec<WorkItem>> {
        // ETag-cached REST listing (#4428), replacing the per-tick GraphQL
        // `gh issue list`. Every page of it (#11139): a repo with more than
        // 100 ready issues no longer starves its oldest ones. Page 1 failing
        // fails the listing; any later shortfall keeps the rows read and
        // marks this repo's queue incomplete.
        self.complete = true;
        let mut items = self.list_label("loom:issue", true)?;
        // No priority side listing (#11508, operator ruling): priority changes
        // order, never eligibility, so a `loom:important` /
        // `loom:very-important` issue is dispatched only once it is
        // `loom:issue`, like everything else. No starred-at or star
        // inheritance is resolved here either.
        // #10118: marker-bearing fixes still in triage / curated. The lane drops
        // them again unless this repo's `main` is red (`main_red_fix::evaluate`).
        // Only a trusted filer's marker counts (#9548); the policy is resolved
        // at most once per tick, and only if a marker-bearing row needs it.
        let mut policy: Option<crate::comment_trust::TrustPolicy> = None;
        let root = self.cwd.clone().unwrap_or_else(|| PathBuf::from("."));
        // First page only (#11139): these listings are large (this repo has
        // ~300 `loom:triage`) and churn on every filing, so walking them would
        // cost several requests a tick for rows the lane drops unless `main`
        // is red; a fresh fix filing sorts first (newest first).
        for label in super::main_red_fix::UNPROMOTED_LABELS {
            let rows = self.list_side_label(label, false);
            items = super::main_red_fix::merge_red_fix_candidates(items, rows, |author| {
                policy
                    .get_or_insert_with(|| crate::comment_trust::TrustPolicy::for_root(&root))
                    .trusts(author)
            });
        }
        Ok(items)
    }
}

impl GhWorkSource {
    /// A listing other than `loom:issue` (the unpromoted red-main fixes, #10118). Its failure
    /// never costs the `loom:issue` rows: log, feed the rate-limit breaker,
    /// and carry on without it, but the repo's queue is then incomplete
    /// (#11139). `all_pages` as [`Self::list_label`].
    fn list_side_label(&mut self, label: &str, all_pages: bool) -> Vec<WorkItem> {
        self.list_label(label, all_pages).unwrap_or_else(|e| {
            log::warn!("work_finder: listing {label} issues failed ({e}); skipping that listing");
            crate::rate_limit_breaker::global_observe_failure(&e.to_string(), "work_finder");
            self.complete = false;
            Vec::new()
        })
    }

    /// The ETag-cached REST listing of open issues carrying `label` (#4428):
    /// a page where nothing changed costs zero rate limit (304). REST issue
    /// listings include PRs, so `pull_request`-marked rows are dropped.
    ///
    /// `all_pages` (#11139) walks every page; a single-page listing still
    /// makes one request, its page 1 the same cache entry as before. A walk
    /// that fell short after page 1 (a page failed, the page cap, a mid-walk
    /// change) returns what it read and clears [`Self::complete`]; only page
    /// 1 failing is an error. Without `all_pages`, page 1 alone.
    fn list_label(&mut self, label: &str, all_pages: bool) -> Result<Vec<WorkItem>> {
        let (gh, cwd, repo) = (&self.gh_bin, self.cwd.as_deref(), self.repo.as_deref());
        // 2am#4367 P5: the candidate listing from the state mirror when the
        // repo's watermark is within its bound; else today's REST path. The
        // claim itself (`dispatch`) re-reads the forge and never the mirror.
        let listing = if let Some(rows) = mirror_rows(cwd, repo, label, all_pages) {
            crate::forge_listing::PagedListing {
                rows,
                incomplete: None,
            }
        } else if all_pages {
            crate::forge_listing::list_issues_cached_paged_as(
                "work_finder",
                gh,
                cwd,
                repo,
                label,
                "open",
            )?
        } else {
            crate::forge_listing::PagedListing {
                rows: crate::forge_listing::list_issues_cached_as(
                    "work_finder",
                    gh,
                    cwd,
                    repo,
                    label,
                    "open",
                )?,
                incomplete: None,
            }
        };
        if let Some(e) = &listing.incomplete {
            log::warn!(
                "work_finder: the {label} listing is incomplete ({e:#}); using the {} rows read \
                 and marking this repo's queue incomplete",
                listing.rows.len()
            );
            crate::rate_limit_breaker::global_observe_failure(&format!("{e:#}"), "work_finder");
            self.complete = false;
        }
        Ok(listing
            .rows
            .into_iter()
            .filter(|r| !r.is_pull_request)
            // The REST listing already returns `body` (#4827) — carrying it
            // onto the item costs no extra request and lets the re-check
            // interval marker be read without a per-issue `gh issue view`.
            .map(|r| {
                WorkItem::with_created_at(r.number, r.labels, r.created_at)
                    .with_body(r.body)
                    .with_updated_at(r.updated_at)
                    .with_open_sub_issues(r.open_sub_issues)
                    .with_author(r.author.as_deref().map(|login| {
                        crate::comment_trust::Author::new(
                            Some(login),
                            r.author_association.as_deref(),
                        )
                    }))
            })
            .collect())
    }
}

/// A candidate listing from the GitHub state mirror (2am#4367 P5), or `None`
/// (read the forge as today): no mirror live, the repo stale or unknown to
/// it, or any error. Always a [`ReadKind::Discovery`] read.
///
/// [`ReadKind::Discovery`]: crate::forge_events::poll_gate::ReadKind::Discovery
fn mirror_rows(
    cwd: Option<&Path>,
    repo: Option<&str>,
    label: &str,
    all_pages: bool,
) -> Option<Vec<crate::forge_listing::RestIssue>> {
    use crate::forge_mirror::discovery::{list_open, Caller, Pages};
    if !crate::forge_mirror::is_active() {
        return None;
    }
    let pages = if all_pages { Pages::All } else { Pages::First };
    list_open(
        crate::forge_events::poll_gate::ReadKind::Discovery,
        Caller::WorkFinder,
        cwd,
        repo,
        Some(label),
        pages,
    )
}

/// A concrete [`WorkDispatcher`] backed by the daemon [`SweepRegistry`].
///
/// `dispatch()` calls the registry's own `dispatch()` — reusing its
/// idempotency key, `mkdir`-atomic claim lock, and `loom:issue →
/// loom:building` label flip — so the finder never reimplements the race
/// guard. `in_flight()` reads the registry's `Running` / `Pending` entries.
pub struct RegistryDispatcher {
    registry: Arc<Mutex<SweepRegistry>>,
}

impl RegistryDispatcher {
    /// Construct a dispatcher over the shared registry. Production goes
    /// through [`dispatcher_pairs`].
    #[must_use]
    pub fn new(registry: Arc<Mutex<SweepRegistry>>) -> Self {
        Self { registry }
    }

    /// The shared registry behind this dispatcher. Test-only seam so a
    /// restart-survivorship test (#6262) can seed the registry the way the
    /// daemon's own startup pass does — through the real registry, not by
    /// injecting an in-flight set the way the `RecordingDispatcher` fake
    /// allows.
    #[cfg(test)]
    #[must_use]
    pub fn registry_for_test(&self) -> Arc<Mutex<SweepRegistry>> {
        self.registry.clone()
    }
}

impl WorkDispatcher for RegistryDispatcher {
    fn in_flight(&self) -> HashSet<u32> {
        let mut reg = match self.registry.lock() {
            Ok(r) => r,
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                return HashSet::new();
            }
        };
        // Reap-on-read (Issue #3893): reconcile liveness before seeding
        // occupancy so a sweep whose child has exited does not over-count
        // against the concurrency budget and defer legitimate new dispatch.
        reg.reap_liveness();
        let mut set = HashSet::new();
        for state in [SweepState::Running, SweepState::Pending] {
            for info in reg.list(Some(&state)) {
                // A step holds its item's slot like a sweep (#11898).
                if let Some(n) = info.kind.item_number() {
                    set.insert(n);
                }
            }
        }
        set
    }

    fn quarantined(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.quarantined_issues(),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// Issues inside a live per-issue dispatch-backoff window (Issue #4485).
    /// Pure in-memory read of the registry state the reaper maintains — no
    /// forge round trip, mirroring `quarantined()`.
    fn backed_off(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.dispatch_backoff_issues(chrono::Utc::now()),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// [`WorkDispatcher::dispatch_backoff_expiry`] (Issue #9311): every
    /// window [`Self::backed_off`] skips on: the later of this host's own end
    /// and a peer-armed window's broadcast end (#11564).
    fn dispatch_backoff_expiry(&self) -> HashMap<u32, chrono::DateTime<chrono::Utc>> {
        match self.registry.lock() {
            Ok(reg) => {
                let now = chrono::Utc::now();
                reg.dispatch_backoff_issues(now)
                    .into_iter()
                    .filter_map(|issue| {
                        // The skip lasts until the later of the two windows.
                        let local = reg.dispatch_backoff_until(issue, now);
                        let until = local
                            .into_iter()
                            .chain(reg.peer_dispatch_backoff_until(issue, now))
                            .max();
                        until.map(|u| (issue, u))
                    })
                    .collect()
            }
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashMap::new()
            }
        }
    }

    /// The subset of `backed_off()` whose window was armed by the
    /// open-PR guard rather than a real dispatch failure (Issue #7606).
    /// Pure in-memory read, mirroring `backed_off()`.
    fn pr_open_backed_off(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.open_pr_backoff_issues(chrono::Utc::now()),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// Issues inside a live no-op re-dispatch cooldown window (Issue
    /// #6670). Pure in-memory read of the registry state a
    /// `RecordNoopRelease` call maintains — no forge round trip,
    /// mirroring `backed_off()`.
    fn noop_cooldown(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.noop_cooldown_issues(chrono::Utc::now()),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// [`WorkDispatcher::noop_cooldown_expiry`] (Issue #9311): like
    /// [`Self::dispatch_backoff_expiry`], the later of the local and the
    /// peer-armed window's end (#11564).
    fn noop_cooldown_expiry(&self) -> HashMap<u32, chrono::DateTime<chrono::Utc>> {
        match self.registry.lock() {
            Ok(reg) => {
                let now = chrono::Utc::now();
                reg.noop_cooldown_issues(now)
                    .into_iter()
                    .filter_map(|issue| {
                        // The skip lasts until the later of the two windows.
                        let local = reg.noop_cooldown_until(issue, now);
                        let until = local
                            .into_iter()
                            .chain(reg.peer_noop_cooldown_until(issue, now))
                            .max();
                        until.map(|u| (issue, u))
                    })
                    .collect()
            }
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashMap::new()
            }
        }
    }

    /// Issues inside a live hard-exclusion decline cooldown window (Issue
    /// #7528). Pure in-memory read of the registry state the reaper's
    /// checkpoint-less clean-exit path maintains — no forge round trip,
    /// mirroring `noop_cooldown()`.
    fn declined(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.decline_cooldown_issues(chrono::Utc::now()),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// [`WorkDispatcher::declined_expiry`] (Issue #9311), mirroring
    /// [`Self::dispatch_backoff_expiry`]'s shape — `decline_cooldown_issues`
    /// is not fleet-unioned (see its own doc comment), so this is a complete
    /// per-issue expiry map, not a this-host-only subset.
    fn declined_expiry(&self) -> HashMap<u32, chrono::DateTime<chrono::Utc>> {
        match self.registry.lock() {
            Ok(reg) => {
                let now = chrono::Utc::now();
                reg.decline_cooldown_issues(now)
                    .into_iter()
                    .filter_map(|issue| reg.decline_cooldown_until(issue, now).map(|u| (issue, u)))
                    .collect()
            }
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashMap::new()
            }
        }
    }

    /// Issues inside a live PR-less retry window (Issue #7972). Pure
    /// in-memory read of the registry state `reap_once`'s terminal-outcome
    /// classification maintains — no forge round trip, mirroring
    /// `noop_cooldown()`.
    fn prless_retry(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.prless_retry_issues(chrono::Utc::now()),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// [`WorkDispatcher::prless_retry_expiry`] (Issue #9311), mirroring
    /// [`Self::declined_expiry`]'s shape — `prless_retry_issues` is not
    /// fleet-unioned either, so this is a complete per-issue expiry map.
    fn prless_retry_expiry(&self) -> HashMap<u32, chrono::DateTime<chrono::Utc>> {
        match self.registry.lock() {
            Ok(reg) => {
                let now = chrono::Utc::now();
                reg.prless_retry_issues(now)
                    .into_iter()
                    .filter_map(|issue| reg.prless_retry_until(issue, now).map(|u| (issue, u)))
                    .collect()
            }
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashMap::new()
            }
        }
    }

    /// Whether this workspace is missing `.claude/commands/loom/sweep.md`
    /// (Issue #4027 guard 2.4, quarantined at the work-finder level by
    /// #6440). A cheap `stat` via `SweepRegistryConfig::has_sweep_command`
    /// — no forge round trip, no lock contention beyond the same mutex
    /// every other dispatcher method already takes.
    ///
    /// Mirrors `dispatch_inner`'s own `!self.config.skip_label_flip &&
    /// !self.config.has_sweep_command()` gate exactly: `skip_label_flip`
    /// marks a hermetic unit-test fixture that never installs
    /// `.claude/commands/loom/` on disk, so without this term every such
    /// fixture would read as workspace-commands-missing and this
    /// pre-filter would silently zero out their candidate batches —
    /// unlike the guard in `dispatch()` itself, which they never actually
    /// reach (label flips, and thus this guard, are the thing they're
    /// opting out of).
    fn workspace_commands_missing(&self) -> bool {
        match self.registry.lock() {
            Ok(reg) => !reg.config().skip_label_flip && !reg.config().has_sweep_command(),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                false
            }
        }
    }

    /// Discounted occupancy count (Issue #4003): a sweep dispatched longer
    /// than the registry's configured startup-proof grace window with zero
    /// observed startup signal does not count toward the budget — see
    /// `SweepRegistry::occupied_issues`. Reap-on-read first, mirroring
    /// `in_flight()`, so a child whose process already exited never
    /// over-counts either.
    fn occupancy(&self) -> usize {
        let mut reg = match self.registry.lock() {
            Ok(r) => r,
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                return 0;
            }
        };
        reg.reap_liveness();
        reg.occupied_issues().len()
    }

    fn collisions(&self) -> u64 {
        match self.registry.lock() {
            Ok(reg) => reg.collision_count(),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                0
            }
        }
    }

    fn peer_claimed(&self) -> HashSet<u32> {
        match self.registry.lock() {
            Ok(reg) => reg.peer_claimed_issues(),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                HashSet::new()
            }
        }
    }

    /// Additional skip-label list for this workspace (Issue #6685),
    /// resolved fresh each call from `<workspace_root>/.loom/config.json`
    /// via [`read_work_finder_config`] / [`resolve_extra_skip_labels_with_config`]
    /// — a cheap JSON read (mirrors `workspace_commands_missing()`'s own
    /// per-call `stat`), so an operator's `autonomous.workFinder.extraSkipLabels`
    /// edit takes effect on the very next tick with no registry-side
    /// config plumbing or daemon restart required.
    fn extra_skip_labels(&self) -> Vec<String> {
        match self.registry.lock() {
            Ok(reg) => resolve_extra_skip_labels_with_config(&read_work_finder_config(
                &reg.config().workspace_root,
            )),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                Vec::new()
            }
        }
    }

    /// Capabilities this **host** declares it holds (#6893), read fresh each
    /// call from `LOOM_WORKER_CAPABILITIES`.
    ///
    /// Deliberately NOT resolved from `.loom/config.json` the way
    /// [`extra_skip_labels`](Self::extra_skip_labels) above is: a skip-label
    /// list is a repo policy, but "this machine has root / an admin token /
    /// a production cloud profile" is a property of the host and its
    /// credentials, and a file committed to git must not be able to assert
    /// it. See [`crate::capability`].
    fn declared_capabilities(&self) -> std::collections::BTreeSet<String> {
        crate::capability::held_capabilities()
    }

    fn dispatch(&mut self, issue: u32) -> Result<bool> {
        self.dispatch_with(issue, false)
    }

    /// One read of this workspace's ETag-cached open-PR listing, classified
    /// for every candidate (#8865); see [`super::open_pr_prefilter::blocked_in`].
    fn open_pr_blocked(&self, candidates: &[u32]) -> std::collections::HashMap<u32, u32> {
        super::open_pr_prefilter::blocked_in(&self.registry, candidates)
    }

    /// The same read with its outcome (#12025); see
    /// [`super::open_pr_prefilter::answer_in`].
    fn open_pr_answer(&self, candidates: &[u32]) -> super::open_pr_prefilter::OpenPrAnswer {
        super::open_pr_prefilter::answer_in(&self.registry, candidates)
    }

    /// Open candidates whose closing PR merged (#11853); see
    /// [`super::landed_prefilter::landed_in`].
    fn landed_closing_prs(&self, candidates: &[u32], open_pr: &[u32]) -> HashMap<u32, u32> {
        super::landed_prefilter::landed_in(&self.registry, candidates, open_pr)
    }

    /// Landed cooldowns to lift early (#11901); see
    /// [`super::landed_prefilter::lift_in`].
    fn lift_landed_cooldowns(&self, held: &[u32]) -> HashSet<u32> {
        super::landed_prefilter::lift_in(&self.registry, held)
    }

    /// Whether a live sweep in this registry is the host's overflow sweep
    /// (#9244). The multi-workspace tick ORs this across every root.
    fn overflow_in_flight(&self) -> bool {
        match self.registry.lock() {
            Ok(reg) => reg.overflow_in_flight(),
            Err(poisoned) => {
                log::error!("work_finder: sweep registry mutex poisoned ({poisoned:?})");
                false
            }
        }
    }

    /// The red-main-fix lane's CI fallback (#9244) for this workspace's repo.
    fn main_red_via_ci(&self) -> bool {
        let root = match self.registry.lock() {
            Ok(reg) => reg.config().workspace_root.clone(),
            Err(_) => return false,
        };
        super::main_red_fix::ci_main_red(&root)
    }

    /// The red-main-fix escalation (#10118) for this workspace's repo; a fix
    /// this registry is already running is not waiting.
    fn escalate_red_fix(&mut self, red: bool, waiting: &[u32]) {
        let root = match self.registry.lock() {
            Ok(reg) => reg.config().workspace_root.clone(),
            Err(_) => return,
        };
        let in_flight = self.in_flight();
        let waiting: Vec<u32> = waiting
            .iter()
            .copied()
            .filter(|n| !in_flight.contains(n))
            .collect();
        super::main_red_fix::escalate_global(&root, red, &waiting);
    }

    /// The red-main auto-filed fix issue (#11766) for this workspace's repo.
    fn autofile_red_main(&mut self, lane: super::RedMainLane) {
        let target = match self.registry.lock() {
            Ok(reg) => reg
                .open_pr_listing_target()
                .map(|(gh, root, nwo)| (root, (gh, nwo))),
            Err(_) => return,
        };
        let Some((root, target)) = target else {
            return;
        };
        let open_prs = |issues: &[u32]| self.open_pr_blocked(issues).into_keys().collect();
        super::main_red_autofile::tick(&root, lane, Some(target), &open_prs);
    }

    fn dispatch_with(&mut self, issue: u32, overflow: bool) -> Result<bool> {
        log::info!("work_finder: attempting issue #{issue}");
        // Resolve the model only after runtime admission. A Claude default must not become a native pin.
        // Idempotency key + the registry's claim lock make a re-dispatch of
        // an already-running issue a no-op (`was_new = false`) or a loud
        // lock-collision error.
        let key = format!("workfinder-{issue}");
        // Issue #6688: extends the #6592 begin/poll/finish split (proven
        // for the IPC `DispatchSweep` handler) to this call site, so the
        // account-selection poll no longer holds the registry mutex a
        // concurrent `DaemonStatus`/`health` IPC call's per-root
        // `registry.lock()` (`ipc.rs::build_daemon_status`) needs.
        let outcome = crate::sweep_registry::dispatch_model_releasing_poll_lock(
            &self.registry,
            &SweepKind::Issue(issue),
            Some(key),
            crate::sweep_registry::DispatchModel::Autonomous,
            None,
            None,
            crate::sweep_registry::DispatchOrigin::WorkFinder,
        )?;
        if overflow && outcome.was_new {
            // Record the over-limit admission on the sweep itself (#9244), so
            // `list_sweeps` / `get_sweep_status` / `loom-daemon status` show it
            // and the next tick sees the slot as taken while it runs.
            if let Ok(mut reg) = self.registry.lock() {
                reg.mark_overflow(&outcome.sweep_id);
            }
            log::info!(
                "work_finder: issue #{issue} dispatched as this host's overflow sweep (#9244)"
            );
        }
        Ok(outcome.was_new)
    }
}

/// Build the per-tick `(source, dispatcher)` pair for every root (#3924).
#[must_use]
pub fn dispatcher_pairs(
    pool: &crate::workspace_pool::WorkspacePool,
    roots: &[PathBuf],
) -> Vec<(GhWorkSource, RegistryDispatcher)> {
    roots
        .iter()
        .map(|root| {
            let registry = pool.get_or_provision(root);
            (GhWorkSource::for_root(root), RegistryDispatcher::new(registry))
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "forge_tests.rs"]
mod tests;
