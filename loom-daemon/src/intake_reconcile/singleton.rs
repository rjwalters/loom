//! Intake reconcile as a fleet-captain singleton (W7 of the fleet GitHub-API
//! reduction plan).
//!
//! # The problem
//!
//! The intake pass describes the *forge*, not the host: every dispatcher that
//! manages a repo lists the same open issues on the same cadence and tries to
//! add the same `loom:triage` label. N hosts spend N paginated listings (on
//! the reader pool since W4-C, shed when the readers run dry) and N label
//! attempts on the writer to do one host's worth of work, which is why fleets
//! turned the pass off per host (`LOOM_INTAKE_RECONCILE=0`) and lost it.
//!
//! The gain is **one producer**, not a smaller bill: against a fleet that
//! already runs with `LOOM_INTAKE_RECONCILE=0` everywhere, turning this on is
//! new reader spend (one walk per repo per pass, on the captain).
//!
//! # The fix: one producer, assigned
//!
//! With `fleet.intakeReconcile.singleton` set and a `fleet.captain` declared
//! (#8848), the captain alone runs the pass, from its own task
//! ([`spawn_task`]) rather than the work finder's tick, so a captain that
//! dispatches nothing still runs it. Every other host stops running intake.
//! Assigned, not elected: there is no standby producer, and a captain that is
//! down means unlabelled issues wait until it is back. Late, never doubled.
//!
//! # What one captain pass costs
//!
//! - **Listing**: every registered repo's open issues through
//!   [`crate::forge_listing::list_open_issues_cached_all_as`]: conditional
//!   reads on the reader pool, one validator per page, as a deferrable
//!   (`Hygiene`) read. A page whose content did not move is a `304`, but on a
//!   busy repo most walks still pay `200`s for the pages that did. When every
//!   reader that can see the repo is out of budget the walk is **shed**: no
//!   request, never a writer retry, and the repo waits for the next pass
//!   (`intake.listing_shed`).
//! - **Re-read**: before each label, one unconditional read of that issue
//!   confirms it still carries no `loom:*` label, so an issue labelled between
//!   the listing and the write is left alone. Unconditional because the
//!   stored validator almost never matches (the issue is a candidate because
//!   it just changed), and its body is not cached.
//! - **Write**: one label request per issue, on the writer, behind a per-repo
//!   [`crate::write_scope`] check that is only made when there is something
//!   to label. The request's response is the issue's label set as the forge
//!   stores it after the add: if it carries any other `loom:*` label (someone
//!   labelled the issue after the re-read), the pass removes the
//!   `loom:triage` it just added (`intake.triage_reverted`).
//!
//! # Repo set
//!
//! The registered workspaces ([`crate::workspace_registry`], the set the work
//! finder fans out over), each reduced to its repo slug from its own `origin`
//! remote. Nothing else is consulted. No registry, or no root that resolves to
//! a slug, is **no pass** ([`registered_repos`]): the captain never guesses a
//! repo set.
//!
//! **Coverage precondition.** Every other host stands down for *every* repo it
//! manages, but the captain covers only the repos registered on it: a repo
//! managed by a dispatcher and not registered on the captain gets no intake.
//! The covered slugs are logged once when this host becomes the captain and
//! again whenever the set changes. A captain that is down means no intake
//! anywhere; the only signal today is `intake-reconcile` missing from its
//! `host.health.armed_singleton_jobs`.
//!
//! # Modes ([`Mode`], re-read every [`GATE_INTERVAL`], so an edit needs no restart)
//!
//! | `fleet.intakeReconcile.singleton` | `fleet.captain` | this host | intake runs |
//! |---|---|---|---|
//! | unset / `false` | any | any | per host, from the work finder, as before |
//! | `true` | not declared | any | per host, as before (fail-open) |
//! | `true` | declared | the captain | here only, from this task |
//! | `true` | declared | another host | not here |
//!
//! `LOOM_INTAKE_RECONCILE=0` disables the pass on a host in every mode, the
//! captain included. No captain declared is fail-open and unarmed, like the
//! ETA fleet refresh (#10329): a duplicate label attempt costs budget, never
//! correctness, and a single-host install must not lose intake for want of a
//! captain.
//!
//! # Rate limits
//!
//! The pass does not start while the rate-limit breaker is suppressing forge
//! calls, and stops between repos if it trips.
//!
//! - **Listing**: a rate-limited or refused reader is withdrawn and the next
//!   reader asked; with none left for budget the walk is shed, never sent to
//!   the writer. A listing that fails or is shed is a skipped repo, retried
//!   next pass, and is never reported to the host-wide breaker from here.
//! - **Re-read**: a `Gate` read on the shared path, so a rate-limited reader
//!   is withdrawn and the read retried once on the writer. A re-read failure
//!   **does** reach the breaker, by design: it is reported only after the
//!   reader was withdrawn and the writer retry also failed (for a rate limit,
//!   the writer's own bucket is spent, which is the breaker's business).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use super::{select_unlabeled, IntakeRow, TRIAGE_LABEL};
use crate::claim_reconciliation::gh_call;
use crate::fleet_captain::{self as captain, CaptainGate};
use crate::forge_etag_store as store;

/// The singleton job name this pass arms under `fleet.captain`
/// (`host.health.armed_singleton_jobs`).
pub const SINGLETON_JOB_NAME: &str = "intake-reconcile";
/// Config key: hand the pass to the declared fleet captain.
pub const SINGLETON_KEY: &str = "fleet.intakeReconcile.singleton";
/// How often the mode is re-read. The pass itself keeps the legacy cadence
/// (`LOOM_INTAKE_RECONCILE_INTERVAL_SECS`).
pub const GATE_INTERVAL: Duration = Duration::from_secs(60);
/// Breaker job name and write-scope pass name.
const PASS: &str = "intake_reconcile";
/// `loom.forge.facade.events`: a repo's listing was shed (readers out of
/// budget) and the repo skipped this pass.
pub const LISTING_SHED: &str = "intake.listing_shed";
/// `loom.forge.facade.events`: the label response showed another `loom:*`
/// label, so the `loom:triage` just added was removed again.
pub const TRIAGE_REVERTED: &str = "intake.triage_reverted";

/// Where intake runs, for this host, this tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Per host, from the work finder, exactly as before; `reason` says why.
    Legacy { reason: &'static str },
    /// This host is the declared captain: armed, and only it runs the pass.
    Captain { captain: String },
    /// Another host is the captain: no intake call from this host.
    StandDown { captain: String },
}

impl Mode {
    /// Whether the work finder's inline pass still runs on this host.
    #[must_use]
    pub fn legacy_runs(&self) -> bool {
        matches!(self, Self::Legacy { .. })
    }
}

/// `fleet.intakeReconcile.singleton` in `effective`; anything but `true` is off.
#[must_use]
pub fn singleton_enabled(effective: &Value) -> bool {
    crate::config_resolver::get_path(effective, SINGLETON_KEY)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The mode for one tick. Pure.
#[must_use]
pub fn resolve_mode(gate: &CaptainGate, singleton: bool) -> Mode {
    match gate {
        _ if !singleton => Mode::Legacy {
            reason: "not_configured",
        },
        CaptainGate::NoCaptainDeclared => Mode::Legacy {
            reason: "no_captain",
        },
        CaptainGate::Armed { captain } => Mode::Captain {
            captain: captain.clone(),
        },
        CaptainGate::Refused { captain, .. } => Mode::StandDown {
            captain: captain.clone(),
        },
    }
}

/// Resolve `root`'s mode for `host_id` and keep `job`'s entry in the
/// armed-singleton registry in step with it.
fn gate_as(job: &str, root: &Path, host_id: &str) -> Mode {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let gate = captain::resolve_gate_for_root(root, host_id);
    let mode = resolve_mode(&gate, singleton_enabled(&effective));
    match &mode {
        Mode::Captain { captain: name } => {
            if captain::arm_singleton_job(job, root, host_id).is_err() {
                // `fleet.captain` changed between the two reads: sit this tick
                // out; the next one re-reads it.
                return Mode::StandDown {
                    captain: name.clone(),
                };
            }
        }
        // Not `arm_singleton_job`: a refusal there with no captain declared
        // would list the job as stopped for want of one, and this one keeps
        // running per host instead.
        Mode::Legacy { .. } | Mode::StandDown { .. } => captain::disarm_singleton_job(job),
    }
    mode
}

/// The last mode [`gate_tick`] resolved. `None` until the daemon's task has
/// resolved one (a CLI process, a unit test): the legacy pass runs.
static MODE: Mutex<Option<Mode>> = Mutex::new(None);

/// One tick's gate: resolve the mode, arm or disarm [`SINGLETON_JOB_NAME`],
/// publish it for [`legacy_stands_down`], and log it when it changes.
pub fn gate_tick(root: &Path, host_id: &str) -> Mode {
    let mode = gate_as(SINGLETON_JOB_NAME, root, host_id);
    let mut current = MODE.lock().unwrap_or_else(PoisonError::into_inner);
    if current.as_ref() != Some(&mode) {
        log_mode(&mode, host_id);
        *current = Some(mode.clone());
    }
    mode
}

/// Whether the work finder's inline pass must not run on this host: the
/// singleton is configured and a captain is declared, so the captain's task
/// (on this host or another) owns intake.
#[must_use]
pub fn legacy_stands_down() -> bool {
    stands_down(MODE.lock().unwrap_or_else(PoisonError::into_inner).as_ref())
}

/// [`legacy_stands_down`] for a given last-resolved mode. Pure.
#[must_use]
pub fn stands_down(mode: Option<&Mode>) -> bool {
    mode.is_some_and(|m| !m.legacy_runs())
}

fn log_mode(mode: &Mode, host_id: &str) {
    match mode {
        Mode::Legacy { reason } => log::info!(
            "intake_reconcile: this host ({host_id}) runs intake per host, from the work finder \
             ({reason})"
        ),
        Mode::Captain { .. } if !super::enabled() => log::warn!(
            "intake_reconcile: this host ({host_id}) is the fleet captain with {SINGLETON_KEY}, \
             but LOOM_INTAKE_RECONCILE disables the pass here: no host in the fleet runs intake \
             until it is removed from this host's environment (W7)"
        ),
        Mode::Captain { .. } => log::info!(
            "intake_reconcile: this host ({host_id}) is the fleet captain with {SINGLETON_KEY}: \
             it alone labels unlabelled issues, for every registered repo (W7)"
        ),
        Mode::StandDown { captain } => log::info!(
            "intake_reconcile: standing down — the fleet captain is {captain} and \
             {SINGLETON_KEY} is set: no intake call from this host ({host_id}) (W7)"
        ),
    }
}

// ---------------------------------------------------------------------------
// Repo set
// ---------------------------------------------------------------------------

/// One repo the captain reconciles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// `owner/repo`.
    pub slug: String,
    /// The forge host of the workspace's remote; `None` = the default host.
    pub host: Option<String>,
    /// The registered workspace whose credential and write scope apply.
    pub root: PathBuf,
}

/// One repo per distinct slug among `roots`, ordered by slug; the first root
/// that resolves to a slug is the one it is reached through. A root with no
/// resolvable slug is left out, never guessed. Pure given `resolve`.
#[must_use]
pub fn repos_for(
    roots: &[PathBuf],
    resolve: impl Fn(&Path) -> Option<(Option<String>, String)>,
) -> Vec<Repo> {
    let mut by_slug: BTreeMap<String, Repo> = BTreeMap::new();
    for root in roots {
        let Some((host, slug)) = resolve(root) else {
            log::debug!("intake_reconcile: {} has no resolvable repo; skipped", root.display());
            continue;
        };
        by_slug
            .entry(slug.to_ascii_lowercase())
            .or_insert_with(|| Repo {
                slug,
                host,
                root: root.clone(),
            });
    }
    by_slug.into_values().collect()
}

/// The `LOOM_REPO` override every listing honours, when set.
fn env_repo() -> Option<String> {
    std::env::var("LOOM_REPO")
        .ok()
        .filter(|r| !r.trim().is_empty())
}

/// The registered workspaces' repos, or `None` when the registry cannot be
/// read: no slug list, no pass.
#[must_use]
pub fn registered_repos(daemon_root: &Path) -> Option<Vec<Repo>> {
    let registry = match crate::workspace_registry::WorkspaceRegistry::load_default() {
        Ok(registry) => registry,
        Err(e) => {
            log::warn!("intake_reconcile: workspace registry unreadable, no pass: {e:#}");
            return None;
        }
    };
    let roots: Vec<PathBuf> = registry
        .effective_roots(daemon_root)
        .into_iter()
        .filter(|root| root.is_dir())
        .collect();
    let repo = env_repo();
    Some(repos_for(&roots, |root| {
        // The same resolution the listing itself applies, so the slug named
        // in the write is the repo that was listed.
        let target = store::resolve_target(Some(root), repo.as_deref());
        target.repo.map(|slug| (target.host, slug))
    }))
}

// ---------------------------------------------------------------------------
// The pass
// ---------------------------------------------------------------------------

/// The forge calls of one pass, injected so the pass is testable offline.
pub trait IntakeForge {
    /// Every open issue (never a pull request) of `repo`.
    ///
    /// # Errors
    /// The listing is incomplete or failed: the repo is skipped this pass.
    fn list_open(&mut self, repo: &Repo) -> Result<Vec<IntakeRow>>;

    /// Issue `number`'s labels as of now, or `None` when it is no longer an
    /// open issue (closed, deleted, or a pull request).
    ///
    /// # Errors
    /// The read failed: nothing is labelled on a guess.
    fn current_labels(&mut self, repo: &Repo, number: u32) -> Result<Option<Vec<String>>>;

    /// May this host write to `repo`? Asked once per repo, and only when
    /// there is something to label.
    fn may_write(&mut self, repo: &Repo) -> bool;

    /// Add [`TRIAGE_LABEL`] to issue `number`: the issue's label set as the
    /// forge answered the write (`Some(vec![])` when the answer did not
    /// parse), or `None` on any failure.
    fn add_triage(&mut self, repo: &Repo, number: u32) -> Option<Vec<String>>;

    /// Remove [`TRIAGE_LABEL`] from issue `number`; already absent is
    /// success. `false` on any failure.
    fn remove_triage(&mut self, repo: &Repo, number: u32) -> bool;
}

/// What one repo's pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RepoReport {
    /// Issues given [`TRIAGE_LABEL`].
    pub labelled: usize,
    /// Issues the listing showed unlabelled that the re-read showed labelled,
    /// closed or gone: left alone.
    pub raced: usize,
    /// Labelled, then un-labelled: the write's answer showed another
    /// `loom:*` label.
    pub reverted: usize,
    /// The listing failed or was incomplete.
    pub listing_failed: bool,
    /// The listing was shed: every reader was out of budget. Implies
    /// `listing_failed`.
    pub listing_shed: bool,
    /// The write-scope check refused the repo.
    pub denied: bool,
}

fn has_lifecycle_label(labels: &[String]) -> bool {
    labels.iter().any(|l| l.starts_with("loom:"))
}

/// A `loom:*` label other than [`TRIAGE_LABEL`].
fn has_other_lifecycle_label(labels: &[String]) -> bool {
    labels
        .iter()
        .any(|l| l.starts_with("loom:") && l != TRIAGE_LABEL)
}

/// One repo: list, select, and for each selected issue re-read then label.
pub fn run_repo(
    forge: &mut dyn IntakeForge,
    repo: &Repo,
    now: DateTime<Utc>,
    cap: usize,
) -> RepoReport {
    let mut report = RepoReport::default();
    let rows = match forge.list_open(repo) {
        Ok(rows) => rows,
        Err(e) if e.downcast_ref::<store::ReadShed>().is_some() => {
            // Logged (rate-limited) and booked `o=shed` by the read path.
            log::debug!("intake_reconcile: {} listing shed this pass: {e:#}", repo.slug);
            crate::forge_call_stats::counters::bump(LISTING_SHED);
            report.listing_failed = true;
            report.listing_shed = true;
            return report;
        }
        Err(e) => {
            log::warn!("intake_reconcile: {} listing skipped this pass: {e:#}", repo.slug);
            report.listing_failed = true;
            return report;
        }
    };
    let picked = select_unlabeled(&rows, now, cap);
    if picked.is_empty() {
        return report;
    }
    // Forge-write scope (#9548): never label issues in a repo this host may
    // not write to. After the listing, so an idle repo costs no probe.
    if !forge.may_write(repo) {
        report.denied = true;
        return report;
    }
    for number in picked {
        // The listing is a snapshot: confirm, immediately before the write,
        // that nobody labelled (or closed) the issue since.
        match forge.current_labels(repo, number) {
            Ok(Some(labels)) if !has_lifecycle_label(&labels) => {}
            Ok(_) => {
                report.raced += 1;
                continue;
            }
            Err(e) => {
                log::warn!(
                    "intake_reconcile: could not re-read {}#{number}; nothing labelled on a \
                     guess: {e:#}",
                    repo.slug
                );
                break;
            }
        }
        let Some(after) = forge.add_triage(repo, number) else {
            log::warn!("intake_reconcile: failed to label {}#{number}", repo.slug);
            break; // likely rate limited / auth; retry next pass
        };
        if !has_other_lifecycle_label(&after) {
            report.labelled += 1;
            continue;
        }
        // Labelled by someone else between the re-read and the write: the
        // write's own answer (the primary's label set) is the compare step.
        report.reverted += 1;
        crate::forge_call_stats::counters::bump(TRIAGE_REVERTED);
        if forge.remove_triage(repo, number) {
            log::info!(
                "intake_reconcile: {}#{number} was labelled concurrently; removed the \
                 {TRIAGE_LABEL} just added",
                repo.slug
            );
        } else {
            log::warn!(
                "intake_reconcile: {}#{number} was labelled concurrently and removing the \
                 {TRIAGE_LABEL} just added failed; it carries both until curated",
                repo.slug
            );
            break;
        }
    }
    report
}

/// What one pass did, over every repo.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Repos whose listing was read.
    pub repos: usize,
    pub labelled: usize,
    pub raced: usize,
    /// Labels removed again after the write's answer (see [`RepoReport`]).
    pub reverted: usize,
    /// Repos whose listing was shed (readers out of budget).
    pub shed: usize,
}

/// One pass over `repos`, stopping early while `suppressed` (the rate-limit
/// breaker) says so.
pub fn run_pass(
    forge: &mut dyn IntakeForge,
    repos: &[Repo],
    now: DateTime<Utc>,
    cap: usize,
    suppressed: &dyn Fn() -> bool,
) -> PassReport {
    let mut pass = PassReport::default();
    for repo in repos {
        if suppressed() {
            log::debug!("intake_reconcile: rate-limit breaker open; pass stopped early");
            break;
        }
        let report = run_repo(forge, repo, now, cap);
        pass.repos += usize::from(!report.listing_failed);
        pass.labelled += report.labelled;
        pass.raced += report.raced;
        pass.reverted += report.reverted;
        pass.shed += usize::from(report.listing_shed);
    }
    pass
}

// ---------------------------------------------------------------------------
// The live forge
// ---------------------------------------------------------------------------

/// The live calls: reader-pool conditional reads, writer label requests.
pub struct GhIntakeForge {
    gh_bin: PathBuf,
}

impl GhIntakeForge {
    #[must_use]
    pub fn new(gh_bin: PathBuf) -> Self {
        Self { gh_bin }
    }
}

/// The names in a label-array answer (`POST …/labels`).
fn label_names(body: &str) -> Option<Vec<String>> {
    let labels: Vec<Value> = serde_json::from_str(body.trim()).ok()?;
    Some(
        labels
            .iter()
            .filter_map(|l| l["name"].as_str().map(str::to_owned))
            .collect(),
    )
}

/// An issue row's labels when it is still an open issue, else `None`.
fn open_issue_labels(body: &str) -> Result<Option<Vec<String>>> {
    let issue: Value = serde_json::from_str(body.trim()).context("parse the issue")?;
    if issue.get("pull_request").is_some_and(|p| !p.is_null())
        || !issue["state"]
            .as_str()
            .is_some_and(|s| s.eq_ignore_ascii_case("open"))
    {
        return Ok(None);
    }
    let labels = issue["labels"]
        .as_array()
        .ok_or_else(|| anyhow!("the issue carries no labels array"))?
        .iter()
        .filter_map(|l| l["name"].as_str().map(str::to_owned))
        .collect();
    Ok(Some(labels))
}

impl IntakeForge for GhIntakeForge {
    fn list_open(&mut self, repo: &Repo) -> Result<Vec<IntakeRow>> {
        let rows = crate::forge_listing::list_open_issues_cached_all_as(
            "intake.list_open",
            &self.gh_bin,
            Some(&repo.root),
            None,
        )?;
        Ok(rows
            .into_iter()
            .filter(|row| !row.is_pull_request)
            .map(|row| IntakeRow {
                number: row.number,
                created_at: row
                    .created_at
                    .as_deref()
                    .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                    .map(|d| d.with_timezone(&Utc)),
                labels: row.labels,
            })
            .collect())
    }

    fn current_labels(&mut self, repo: &Repo, number: u32) -> Result<Option<Vec<String>>> {
        const CALLER: &str = "intake.recheck";
        let site =
            store::ConditionalRead::new(CALLER, crate::forge_call_stats::ops::ISSUE_VIEW_STATE)
                .item_scoped();
        if crate::rate_limit_breaker::global_skip_pass(CALLER) {
            anyhow::bail!("rate-limit breaker is suppressing forge calls");
        }
        // Unconditional and uncached: a candidate issue has usually just
        // changed, so a stored validator would rarely match, and keeping its
        // body would grow an unevicted cache by one issue per label.
        let target = store::resolve_target(Some(&repo.root), env_repo().as_deref());
        let url = format!("repos/{}/issues/{number}", repo.slug);
        let (status, response, stderr) =
            store::fetch_conditional(site, &self.gh_bin, Some(&repo.root), &target, &url, None)?;
        match response {
            Some(r) if r.status == 200 && status.success() => open_issue_labels(&r.body),
            Some(r) if r.status == 404 => Ok(None),
            _ => {
                // After the reader was withdrawn and the writer retry failed.
                crate::rate_limit_breaker::global_observe_failure(&stderr, CALLER);
                Err(anyhow!("gh api {url} failed: {stderr}"))
            }
        }
    }

    fn may_write(&mut self, repo: &Repo) -> bool {
        crate::write_scope::gate_repo_with(&repo.root, &repo.slug, &self.gh_bin, "intake reconcile")
    }

    fn add_triage(&mut self, repo: &Repo, number: u32) -> Option<Vec<String>> {
        let path = format!("repos/{}/issues/{number}/labels", repo.slug);
        let label = format!("labels[]={TRIAGE_LABEL}");
        let mut call = gh_call::write("intake.add_triage", &self.gh_bin, &repo.root)
            .args(["api", "-X", "POST", &path, "-f", &label]);
        if let Some(host) = &repo.host {
            call = call.arg("--hostname").arg(host);
        }
        let out = gh_call::output(call).ok().filter(|o| o.status.success())?;
        // The answer is the issue's whole label set. One that does not parse
        // proves nothing either way: the label was added, nothing to undo.
        Some(label_names(&String::from_utf8_lossy(&out.stdout)).unwrap_or_default())
    }

    fn remove_triage(&mut self, repo: &Repo, number: u32) -> bool {
        let path = format!("repos/{}/issues/{number}/labels/{TRIAGE_LABEL}", repo.slug);
        let mut call = gh_call::write("intake.remove_triage", &self.gh_bin, &repo.root)
            .args(["api", "-X", "DELETE", &path]);
        if let Some(host) = &repo.host {
            call = call.arg("--hostname").arg(host);
        }
        match gh_call::output(call) {
            Ok(o) if o.status.success() => true,
            // Already absent is the end state we want.
            Ok(o) => {
                let err = gh_call::stderr(&o);
                err.contains("404") || err.to_ascii_lowercase().contains("does not exist")
            }
            Err(_) => false,
        }
    }
}

// ---------------------------------------------------------------------------
// The task
// ---------------------------------------------------------------------------

/// The captain's pass over the registered repos, behind the host's kill
/// switch and the rate-limit breaker.
fn run_production_pass(daemon_root: &Path) {
    if !super::enabled() || crate::rate_limit_breaker::global_skip_pass(PASS) {
        return;
    }
    let repos = registered_repos(daemon_root).unwrap_or_default();
    log_coverage(&repos);
    if repos.is_empty() {
        log::warn!(
            "intake_reconcile: no registered workspace resolves to a repo; no pass (the captain \
             reconciles only the workspaces registered on it)"
        );
        return;
    }
    let mut forge = GhIntakeForge::new(PathBuf::from(crate::gh_invocation::gh_bin()));
    let cap = super::env_num("LOOM_INTAKE_RECONCILE_MAX_PER_PASS", super::DEFAULT_MAX_PER_PASS);
    let pass = run_pass(
        &mut forge,
        &repos,
        Utc::now(),
        cap,
        &crate::rate_limit_breaker::global_is_suppressed,
    );
    if pass.labelled > 0 || pass.raced > 0 || pass.reverted > 0 {
        log::info!(
            "intake_reconcile: applied {TRIAGE_LABEL} to {} unlabelled issue(s) across {} \
             repo(s); {} left alone after the re-read, {} removed again after the write \
             (#10041, W7)",
            pass.labelled,
            pass.repos,
            pass.raced,
            pass.reverted
        );
    }
    if pass.shed > 0 {
        log::info!(
            "intake_reconcile: {} repo(s) skipped this pass: every reader was out of budget \
             (shed, not sent to the writer)",
            pass.shed
        );
    }
}

/// The slugs last logged as covered; `None` while this host is not the
/// captain, so becoming it logs the set once.
static COVERED: Mutex<Option<Vec<String>>> = Mutex::new(None);

/// `repos`' slugs when they differ from `last` (and records them), else
/// `None`. Pure apart from `last`.
fn coverage_changed(last: &mut Option<Vec<String>>, repos: &[Repo]) -> Option<Vec<String>> {
    let slugs: Vec<String> = repos.iter().map(|r| r.slug.clone()).collect();
    if last.as_ref() == Some(&slugs) {
        return None;
    }
    *last = Some(slugs.clone());
    Some(slugs)
}

/// Log the repos this captain covers, once per change of the set.
fn log_coverage(repos: &[Repo]) {
    let mut last = COVERED.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(slugs) = coverage_changed(&mut last, repos) {
        log::info!(
            "intake_reconcile: the captain covers {} repo(s): [{}]. Every other host stands \
             down for every repo it manages; a repo missing here gets no intake (register it on \
             this host) (W7)",
            slugs.len(),
            slugs.join(", ")
        );
    }
}

/// One task tick: the gate, then the pass when this host is the captain and
/// the pass is due. `last_pass` is cleared whenever this host is not the
/// captain, so taking the job over starts with a pass.
fn tick(daemon_root: &Path, host_id: &str, last_pass: &mut Option<Instant>) {
    if !matches!(gate_tick(daemon_root, host_id), Mode::Captain { .. }) {
        *last_pass = None;
        *COVERED.lock().unwrap_or_else(PoisonError::into_inner) = None;
        return;
    }
    let interval =
        super::env_num("LOOM_INTAKE_RECONCILE_INTERVAL_SECS", super::DEFAULT_INTERVAL_SECS);
    if last_pass.is_some_and(|at| at.elapsed().as_secs() < interval) {
        return;
    }
    *last_pass = Some(Instant::now());
    run_production_pass(daemon_root);
}

/// Start the task. Always spawned: on a host that is not the captain (or with
/// the singleton unconfigured) each tick is a config read and nothing else.
///
/// The mode is resolved once before returning, so the work finder's first
/// tick already knows whether its inline pass stands down.
pub fn spawn_task(daemon_root: PathBuf) -> tokio::task::JoinHandle<()> {
    gate_tick(&daemon_root, &crate::sweep_registry::host_identity());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(GATE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ticker.tick().await; // immediate: the gate was just resolved
        let mut last_pass: Option<Instant> = None;
        loop {
            ticker.tick().await;
            let root = daemon_root.clone();
            // Re-resolved every tick, like `ci_telemetry`'s gate (#8848).
            let host_id = crate::sweep_registry::host_identity();
            let mut state = last_pass;
            let ticked = tokio::task::spawn_blocking(move || {
                tick(&root, &host_id, &mut state);
                state
            })
            .await;
            match ticked {
                Ok(state) => last_pass = state,
                Err(e) => log::warn!("intake_reconcile: pass panicked; retrying next tick: {e}"),
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "singleton_tests.rs"]
mod tests;
