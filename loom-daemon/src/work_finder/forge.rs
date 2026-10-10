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
    operator_priority, read_work_finder_config, resolve_extra_skip_labels_with_config,
    WorkDispatcher, WorkItem, WorkSource,
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
        if let Some(root) = self.cwd.as_deref() {
            crate::intake_reconcile::maybe_run(&self.gh_bin, root);
            // The `loom:blocked` release pass (#10556) has its own task since
            // #10763: `crate::stale_blocked::release_task`.
        }
        // ETag-cached REST listing (#4428), replacing the per-tick GraphQL
        // `gh issue list`. Every page of it (#11139): a repo with more than
        // 100 ready issues no longer starves its oldest ones. Page 1 failing
        // fails the listing; any later shortfall keeps the rows read and
        // marks this repo's queue incomplete.
        self.complete = true;
        let ready = self.list_label("loom:issue", true)?;
        // Second ETag-cached listing (#9244 §4): starred issues outside
        // `loom:issue` (triage, curated, or no workflow label at all). Its
        // failure never costs the `loom:issue` rows: log, feed the rate-limit
        // breaker, and carry on with what the first listing returned.
        // #10307: one listing per level label (operator and inherited, every
        // level): a level-2 issue need not carry the star itself.
        let mut starred: Vec<WorkItem> = Vec::new();
        for label in crate::operator_levels::starred_labels(crate::operator_levels::table()) {
            let rows = self.list_side_label(label, true);
            let seen: HashSet<u32> = starred.iter().map(|i| i.number).collect();
            starred.extend(rows.into_iter().filter(|i| !seen.contains(&i.number)));
        }
        let mut items = operator_priority::merge_starred(ready, starred);
        let key = operator_priority::repo_key(self.cwd.as_deref(), self.repo.as_deref());
        let mut timeline = operator_priority::GhTimelineStarredAt {
            gh_bin: self.gh_bin.clone(),
            cwd: self.cwd.clone(),
            repo: self.repo.clone(),
        };
        // #9244 C: a loom-ui intent's `requested_at` answers first (the seam
        // slice A left), then blockers of starred issues inherit the star.
        let mut source = crate::star_liveness::intents::IntentStarredAt {
            root: self.cwd.as_deref(),
            inner: &mut timeline,
        };
        operator_priority::resolve_starred_at(&key, &mut items, &mut source);
        // #10307: a blocker inheriting a level orders at its source's
        // starred-at, which its body provenance marker carries (the listing
        // already returned the body).
        let table = crate::operator_levels::table();
        for item in &mut items {
            let item_key = (self.repo.clone().unwrap_or_default(), item.number);
            if let Some(at) = crate::star_liveness::levels::inherited_requested_at(
                table,
                &item_key,
                &item.labels,
                item.body.as_deref(),
            ) {
                item.operator_priority_at = Some(at);
            }
        }
        crate::star_liveness::inherit::apply(self.cwd.as_deref(), &mut items);
        // #10118: marker-bearing fixes still in triage / curated. Merged after
        // star inheritance so such a row never looks starred; the lane drops
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

    fn listing_complete(&self) -> bool {
        self.complete
    }
}

impl GhWorkSource {
    /// A listing other than `loom:issue` (#9244 §4, #10118). Its failure
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
        let listing = if all_pages {
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
            // onto the item costs no extra request and lets dispatch read
            // the `<!-- loom:complexity=... -->` stratum without a
            // per-issue `gh issue view`.
            .map(|r| {
                WorkItem::with_created_at(r.number, r.labels, r.created_at)
                    .with_body(r.body)
                    .with_updated_at(r.updated_at)
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
                if let SweepKind::Issue(n) = info.kind {
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

    /// [`WorkDispatcher::dispatch_backoff_expiry`] (Issue #9311): this host's
    /// own local window only, mirroring [`Self::pr_open_backed_off`]'s scope
    /// rather than [`Self::backed_off`]'s fleet union — a peer-armed window's
    /// expiry is not available here.
    fn dispatch_backoff_expiry(&self) -> HashMap<u32, chrono::DateTime<chrono::Utc>> {
        match self.registry.lock() {
            Ok(reg) => {
                let now = chrono::Utc::now();
                reg.dispatch_backoff_issues(now)
                    .into_iter()
                    .filter_map(|issue| reg.dispatch_backoff_until(issue, now).map(|u| (issue, u)))
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

    /// [`WorkDispatcher::noop_cooldown_expiry`] (Issue #9311): this host's own
    /// local window only — a peer-armed window's expiry is not available
    /// here, mirroring [`Self::dispatch_backoff_expiry`]'s scope.
    fn noop_cooldown_expiry(&self) -> HashMap<u32, chrono::DateTime<chrono::Utc>> {
        match self.registry.lock() {
            Ok(reg) => {
                let now = chrono::Utc::now();
                reg.noop_cooldown_issues(now)
                    .into_iter()
                    .filter_map(|issue| reg.noop_cooldown_until(issue, now).map(|u| (issue, u)))
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

    fn dispatch(&mut self, issue: u32, complexity: Option<&str>) -> Result<bool> {
        self.dispatch_with(issue, complexity, false)
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

    fn dispatch_with(
        &mut self,
        issue: u32,
        complexity: Option<&str>,
        overflow: bool,
    ) -> Result<bool> {
        log::info!("work_finder: attempting issue #{issue}");
        // Keep the cached complexity stratum, but resolve its model only after
        // runtime admission. A Claude default must not become a native pin.
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
            crate::sweep_registry::DispatchModel::Autonomous { complexity },
            None,
            None,
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
