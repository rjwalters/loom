//! The liveness pass and its background thread.
//!
//! [`LivenessState::run_pass`] is the whole pass over already-resolved
//! inputs and a forge factory, so the tests (and the 2026-09-28 replay) run
//! it end to end against fakes. [`spawn`] is the thin production shell: it
//! resolves managed repos, the last work-finder tick and this host's pool
//! holds, runs a pass, and publishes the report.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::collect::{self, Evaluated, RefusalCache, RepoContext};
use super::escalate::{self, Ledger, Notice, Outcome, Target};
use super::forge::{GhStarForge, StarForge};
use super::inherit::{self, Inherited};
use super::intents::{self, AppliedIds, StarIntent};
use super::progress::{self, Tracker, Watched};
use super::stale;
use super::Settings;
use crate::types::{
    AskKind, CapView, DroppedStarIntent, LandingStage, OperatorAsk, ReadyQueueRow, StarLandingRow,
    StarLivenessReport,
};

/// Most dropped intents the report keeps.
const MAX_DROPPED: usize = 50;
/// Most unmanaged stars remembered.
const MAX_UNMANAGED: usize = 200;
/// Consecutive failed passes over a repo after which its published
/// inheritance is withdrawn: an unreadable repo must not keep stale blockers
/// at star priority indefinitely.
pub const MAX_FAILED_PASSES: u32 = 3;

/// One managed repo, resolved.
#[derive(Debug, Clone)]
pub struct RepoInput {
    pub root: PathBuf,
    /// Forge `owner/repo`.
    pub slug: String,
    /// The last work-finder tick's rows for this root.
    pub tick_rows: Vec<ReadyQueueRow>,
    /// This host's pool exhaustion for the root's pool (its description).
    pub pool: Option<String>,
    /// The last tick's rows for every repo on this host (#10214), for a
    /// starred issue's host-wide queue position. Empty falls back to
    /// [`Self::tick_rows`].
    pub host_queue: std::sync::Arc<Vec<ReadyQueueRow>>,
    /// The last tick's cap terms, when recorded (#10214).
    pub cap: Option<CapView>,
    /// The forge's web origin (`https://github.com` unless this repo's
    /// `origin` remote names another host), so an escalation's Matrix line
    /// carries a clickable link on a Gitea fleet too (#9321).
    pub web_base: String,
}

/// The web origin every escalation link falls back to.
pub const DEFAULT_WEB_BASE: &str = "https://github.com";

/// Opens a forge for (root, slug).
pub type ForgeFactory<'a> = dyn FnMut(&Path, &str) -> Box<dyn StarForge> + 'a;

/// State that lives across passes.
#[derive(Debug, Default)]
pub struct LivenessState {
    tracker: Tracker,
    ledger: Ledger,
    refusals: RefusalCache,
    applied: AppliedIds,
    /// Stars (from loom-ui) on repos no workspace here manages.
    unmanaged: BTreeMap<(String, u32), Option<String>>,
    dropped: VecDeque<DroppedStarIntent>,
    /// When this host first saw each issue waiting on an exhausted pool (the
    /// grace window's start).
    pool_since: HashMap<(String, u32), DateTime<Utc>>,
    /// Consecutive failed passes per workspace root.
    failed_passes: HashMap<PathBuf, u32>,
}

impl LivenessState {
    /// Take the fleet-comms notices the last [`Self::run_pass`] produced
    /// (#9321) — one per newly-posted escalation, one per ask that cleared.
    /// The caller publishes them on `operator_priority.escalation`; a caller
    /// with no bus drops them, which is the pre-#9321 behavior.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        self.ledger.take_notices()
    }

    fn record_drop(&mut self, intent: &StarIntent, d: DroppedStarIntent) {
        log::warn!(
            "star_liveness: dropped loom-ui intent {} ({} #{} {}): {}",
            d.id,
            d.repo,
            d.number,
            intent.action,
            d.reason
        );
        if d.reason == "unmanaged-repo" {
            let key = (intent.repo.trim().to_string(), intent.number);
            if intent.action == "unstar" {
                self.unmanaged.remove(&key);
            } else if intent.action == "star" && self.unmanaged.len() < MAX_UNMANAGED {
                self.unmanaged.insert(key, intent.requested_at.clone());
            }
        }
        if self.dropped.len() >= MAX_DROPPED {
            self.dropped.pop_front();
        }
        self.dropped.push_back(d);
    }

    fn apply_intents(
        &mut self,
        batch: Vec<StarIntent>,
        managed: &HashMap<String, (String, PathBuf)>,
        write: bool,
        forges: &mut ForgeFactory<'_>,
    ) {
        for intent in batch {
            match intents::validate(&intent, managed) {
                Err(d) => self.record_drop(&intent, d),
                Ok(valid) => {
                    if self.applied.contains(&valid.id) {
                        continue;
                    }
                    if !write {
                        log::debug!(
                            "star_liveness: not applying loom-ui intent {} (escalate: false)",
                            valid.id
                        );
                        continue;
                    }
                    let mut forge = forges(&valid.root, &valid.repo);
                    match intents::apply(forge.as_mut(), &valid) {
                        Ok(done) => {
                            self.applied.insert(&valid.id);
                            log::info!(
                                "star_liveness: applied loom-ui intent {} on {}#{} \
                                 (label changed: {}, commented: {})",
                                valid.id,
                                valid.repo,
                                valid.number,
                                done.label_changed,
                                done.commented
                            );
                        }
                        Err(e) => log::warn!(
                            "star_liveness: applying loom-ui intent {} failed ({e}); the \
                             backend resends it",
                            valid.id
                        ),
                    }
                }
            }
        }
    }

    /// Run one pass. `intents` are this pass's drained loom-ui intents.
    pub fn run_pass(
        &mut self,
        repos: &[RepoInput],
        batch: Vec<StarIntent>,
        settings: Settings,
        host: &str,
        now: DateTime<Utc>,
        forges: &mut ForgeFactory<'_>,
    ) -> StarLivenessReport {
        let managed: HashMap<String, (String, PathBuf)> = repos
            .iter()
            .map(|r| (r.slug.to_ascii_lowercase(), (r.slug.clone(), r.root.clone())))
            .collect();
        self.apply_intents(batch, &managed, settings.escalate, forges);
        // A repo registered since the star arrived is handled normally.
        self.unmanaged
            .retain(|(repo, _), _| !managed.contains_key(&repo.to_ascii_lowercase()));

        let mut evaluated: Vec<(Option<PathBuf>, Evaluated)> = Vec::new();
        let mut failed = Vec::new();
        let is_managed = |slug: &str| managed.contains_key(&slug.to_ascii_lowercase());
        for repo in repos {
            let root = repo.root.clone();
            let recorded = move |n: u32| intents::recorded_starred_at(&root, n);
            let ctx = RepoContext {
                slug: &repo.slug,
                host,
                tick_rows: &repo.tick_rows,
                host_queue: &repo.host_queue,
                cap: repo.cap,
                pool: repo.pool.clone(),
                managed: &is_managed,
                recorded_starred_at: &recorded,
            };
            let mut forge = forges(&repo.root, &repo.slug);
            let result = collect::Evaluator::new(forge.as_mut(), ctx, &mut self.refusals)
                .with_propagate(settings.propagate)
                .run();
            match result {
                Ok(rows) => {
                    let inherited: Vec<Inherited> = rows
                        .iter()
                        .filter_map(|e| {
                            e.inherited_from.map(|from| Inherited {
                                number: e.facts.issue.number,
                                from,
                                starred_at: e.starred_at.clone(),
                                item: e.item.clone(),
                            })
                        })
                        .collect();
                    inherit::publish(&repo.root, inherited);
                    self.failed_passes.remove(&repo.root);
                    evaluated.extend(rows.into_iter().map(|e| (Some(repo.root.clone()), e)));
                }
                Err(e) => {
                    log::warn!("star_liveness: evaluating {} failed: {e}", repo.slug);
                    failed.push(repo.slug.clone());
                    let n = self.failed_passes.entry(repo.root.clone()).or_insert(0);
                    *n += 1;
                    if *n >= MAX_FAILED_PASSES && !inherit::current(&repo.root).is_empty() {
                        log::warn!(
                            "star_liveness: {} unreadable for {n} passes; withdrawing its \
                             inherited stars until a pass succeeds",
                            repo.slug
                        );
                        inherit::publish(&repo.root, Vec::new());
                    }
                }
            }
        }
        for ((slug, number), at) in &self.unmanaged {
            evaluated.push((None, collect::unmanaged(slug, *number, at.clone())));
        }

        self.ledger.host = host.to_string();
        self.ledger.write = settings.escalate;
        let web_bases: HashMap<String, String> = repos
            .iter()
            .map(|r| (r.slug.clone(), r.web_base.clone()))
            .collect();
        let mut rows = Vec::new();
        let mut live = HashSet::new();
        // Every (repo, issue, key) still being asked for this pass — the input
        // to the recovery notice below (#9321).
        let mut asked: HashSet<(String, u32, String)> = HashSet::new();
        let mut posted = 0;
        for (root, mut e) in evaluated {
            let repo = e.facts.repo.clone();
            let issue = e.facts.issue.number;
            live.insert((repo.clone(), issue));
            self.pool_grace(&mut e, now, settings.pools_grace);
            let position = e
                .landing
                .capacity_wait
                .as_ref()
                .filter(|w| e.landing.stage == LandingStage::NoCapacity && w.queued())
                .and_then(|w| w.position);
            let obs =
                self.tracker
                    .observe(&repo, issue, e.landing.stage, &e.fingerprint, position, now);
            let mut ask = e.landing.ask.clone();
            let mut progress_at = obs.progress_at;
            if let (None, Some(root)) = (&ask, &root) {
                ask = self.watch(&e, root, now, settings.no_progress, &mut progress_at, forges);
            }
            if let Some(a) = &ask {
                asked.insert((repo.clone(), issue, a.key.clone()));
            }
            if let (Some(a), Some(root)) = (&ask, &root) {
                let mut forge = forges(root, &repo);
                let web_base = web_bases
                    .get(&repo)
                    .map_or(DEFAULT_WEB_BASE, String::as_str);
                let target = Target {
                    repo: &repo,
                    issue,
                    stage: e.landing.stage,
                    url: escalate::issue_url(web_base, &repo, issue),
                    inherited_from: e.inherited_from,
                };
                match self.ledger.escalate(forge.as_mut(), &target, a) {
                    Ok(Outcome::Posted) => {
                        posted += 1;
                        log::warn!(
                            "star_liveness: escalated {repo}#{issue} to the operator: {}",
                            a.text
                        );
                    }
                    Ok(_) => {}
                    Err(err) => log::warn!(
                        "star_liveness: posting the escalation for {repo}#{issue} failed ({err}); \
                         retrying next pass"
                    ),
                }
            }
            // #10151: a stale `loom:blocked` is resolved here, not escalated.
            if let (Some(action), Some(root), true) = (&e.landing.stale, &root, settings.escalate) {
                let mut forge = forges(root, &repo);
                match stale::apply(
                    forge.as_mut(),
                    issue,
                    &e.facts.issue.labels,
                    action,
                    host,
                    e.inherited_from,
                ) {
                    Ok(()) => log::info!("star_liveness: resolved the stale block on {repo}#{issue}: {action:?}"),
                    Err(err) => log::warn!(
                        "star_liveness: resolving the stale block on {repo}#{issue} failed ({err}); \
                         retrying next pass"
                    ),
                }
            }
            let secs = now.signed_duration_since(obs.stage_since).num_seconds();
            rows.push(StarLandingRow {
                repo,
                issue,
                stage: e.landing.stage,
                next_actor: e.landing.next_actor.clone(),
                stage_since: Some(obs.stage_since),
                time_in_stage_secs: u64::try_from(secs).unwrap_or(0),
                pr: e.landing.pr,
                blocked_by: e.landing.blocked_by.clone(),
                no_capacity: e.landing.no_capacity.clone(),
                capacity_wait: e.landing.capacity_wait.clone(),
                ask,
                inherited_from: e.inherited_from,
                operator_priority_at: e
                    .starred_at
                    .clone()
                    .or_else(|| e.facts.issue.created_at.clone()),
                last_progress_at: Some(progress_at),
            });
        }
        self.tracker.retain(&live, &failed);
        self.pool_since
            .retain(|k, _| live.contains(k) || failed.contains(&k.0));
        // Recovery half of the fleet-comms notice (#9321): a key this process
        // announced and no longer asks for has resolved. Only repos this pass
        // actually read count — an unreadable repo contributes no rows, which
        // must not read as "every ask on it cleared".
        let readable: HashSet<String> = repos
            .iter()
            .map(|r| r.slug.clone())
            .filter(|slug| !failed.contains(slug))
            .collect();
        self.ledger.resolve_cleared(&asked, &readable);
        StarLivenessReport {
            at: Some(now),
            rows: order_rows(rows),
            escalations_posted: posted,
            dropped_intents: self.dropped.iter().cloned().collect(),
            failed_repos: failed,
        }
    }
}

impl LivenessState {
    /// Hold a `pools-exhausted` ask for `grace` after this host first sees
    /// the issue waiting on its exhausted pool: a peer host with capacity may
    /// simply not have ticked yet. Inside the window the row is `no-capacity`
    /// (the work finder's peers may still take it).
    fn pool_grace(&mut self, e: &mut Evaluated, now: DateTime<Utc>, grace: Duration) {
        let key = (e.facts.repo.clone(), e.facts.issue.number);
        let pooled = e
            .landing
            .ask
            .as_ref()
            .is_some_and(|a| a.kind == AskKind::PoolsExhausted);
        if !pooled {
            self.pool_since.remove(&key);
            return;
        }
        let since = *self.pool_since.entry(key).or_insert(now);
        let waited = now
            .signed_duration_since(since)
            .to_std()
            .unwrap_or_default();
        if waited >= grace {
            return;
        }
        let left = (grace - waited).as_secs().div_ceil(60);
        e.landing.stage = LandingStage::NoCapacity;
        e.landing.next_actor = "work-finder".to_string();
        e.landing.ask = None;
        e.landing.capacity_wait = None;
        e.landing.no_capacity = Some(format!(
            "token pool exhausted on this host; waiting ~{left} min for a peer host to claim it \
             before asking the operator"
        ));
    }

    /// The no-progress watchdog for one managed row. Before tripping, the
    /// issue's comments are read once for forge-seen activity (a comment or
    /// a lease renewal) newer than the fingerprint clock, which every host
    /// sees alike. A key this process already escalated needs no read.
    fn watch(
        &mut self,
        e: &Evaluated,
        root: &Path,
        now: DateTime<Utc>,
        window: Duration,
        progress_at: &mut DateTime<Utc>,
        forges: &mut ForgeFactory<'_>,
    ) -> Option<OperatorAsk> {
        let repo = &e.facts.repo;
        let issue = e.facts.issue.number;
        let check = |at: DateTime<Utc>| {
            progress::watchdog(
                &Watched {
                    repo,
                    issue,
                    stage: e.landing.stage,
                    next_actor: &e.landing.next_actor,
                    fingerprint: &e.fingerprint,
                    progress_at: at,
                    wait: e.landing.capacity_wait.as_ref(),
                },
                now,
                window,
            )
        };
        let ask = check(*progress_at)?;
        if self.ledger.knows(repo, issue, &ask.key) {
            return Some(ask);
        }
        let mut forge = forges(root, repo);
        let seen = match forge.comments(issue) {
            Ok(comments) => {
                let me = forge.self_login();
                progress::latest_comment_activity(&comments, me.as_deref())
            }
            Err(err) => {
                // Warn, not debug (#10214 root cause 5): a failed read here
                // means comment activity cannot reset the progress clock, so
                // the escalation that follows may be spurious.
                log::warn!(
                    "star_liveness: reading {repo}#{issue} comments failed ({err}); comment \
                     activity cannot count as progress this pass"
                );
                None
            }
        };
        if let Some(at) = seen.filter(|at| *at > *progress_at) {
            if let Some(moved) = self.tracker.note_progress(repo, issue, at) {
                *progress_at = moved;
            }
            return check(*progress_at);
        }
        Some(ask)
    }
}

/// Starred rows by starred-at (then repo, issue); each inheriting blocker
/// right after the issue it inherits from.
fn order_rows(rows: Vec<StarLandingRow>) -> Vec<StarLandingRow> {
    let (mut roots, children): (Vec<_>, Vec<_>) =
        rows.into_iter().partition(|r| r.inherited_from.is_none());
    roots.sort_by(|a, b| {
        (&a.operator_priority_at, &a.repo, a.issue).cmp(&(
            &b.operator_priority_at,
            &b.repo,
            b.issue,
        ))
    });
    let mut out = Vec::new();
    let mut pending = children;
    for r in roots {
        let key = (r.repo.clone(), r.issue);
        out.push(r);
        let mut stack = vec![key];
        while let Some((repo, parent)) = stack.pop() {
            let (mine, rest): (Vec<_>, Vec<_>) = pending
                .into_iter()
                .partition(|c| c.repo == repo && c.inherited_from == Some(parent));
            pending = rest;
            for c in mine {
                stack.push((c.repo.clone(), c.issue));
                out.push(c);
            }
        }
    }
    out.extend(pending);
    out
}

/// This host's pool exhaustion for `root`, from the work finder's pool holds.
fn pool_for(root: &Path) -> Option<String> {
    let dir = crate::tokens_pool::paths::resolve_tokens_dir(root);
    crate::work_finder::pool_preflight::active_hold_statuses()
        .into_iter()
        .find(|h| h.dir == dir)
        .map(|h| {
            format!(
                "all {} token(s) exhausted since {}, next possible clear ~{}",
                h.total,
                h.since.format("%Y-%m-%d %H:%MZ"),
                h.next_clear_at.format("%H:%MZ")
            )
        })
}

/// The forge's web origin for the repo at `root`, from its `origin` remote, so
/// an escalation link resolves on a Gitea fleet too (#9321). Recognized shapes:
///
/// | Remote | Web base |
/// |---|---|
/// | `https://host/o/r.git` | `https://host` |
/// | `http://host/o/r.git` | `http://host` |
/// | `git@host:o/r.git` (scp-like) | `https://host` |
/// | `ssh://git@host/o/r.git` | `https://host` |
/// | `ssh://git@host:2222/o/r.git` | `https://host` |
///
/// An `ssh://` remote's port is the **SSH** port, never the forge's web port, so
/// it is dropped rather than carried into the URL. Anything unrecognizable — no
/// git, no remote, an odd URL — falls back to [`DEFAULT_WEB_BASE`], which is
/// correct for every GitHub deployment.
#[must_use]
pub fn web_base_from_remote(remote_url: &str) -> String {
    let url = remote_url.trim();
    // `ssh://[user@]host[:port]/owner/repo.git` — the forge still serves the web
    // UI over https on that host, so only the host survives.
    if let Some(rest) = url.strip_prefix("ssh://") {
        let authority = rest.split('/').next().unwrap_or_default();
        let host_port = authority.rsplit('@').next().unwrap_or_default();
        let host = host_port.split(':').next().unwrap_or_default();
        if !host.is_empty() {
            return format!("https://{host}");
        }
    }
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some((host, _)) = rest.split_once(':') {
            if !host.is_empty() {
                return format!("https://{host}");
            }
        }
    }
    for scheme in ["https://", "http://"] {
        if let Some(rest) = url.strip_prefix(scheme) {
            let host = rest.split('/').next().unwrap_or_default();
            if !host.is_empty() {
                return format!("{scheme}{host}");
            }
        }
    }
    DEFAULT_WEB_BASE.to_string()
}

/// [`web_base_from_remote`] for the checkout at `root`.
fn web_base(root: &Path) -> String {
    let out = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .output();
    match out {
        Ok(out) if out.status.success() => {
            web_base_from_remote(&String::from_utf8_lossy(&out.stdout))
        }
        _ => DEFAULT_WEB_BASE.to_string(),
    }
}

/// Resolve the managed repos for the daemon rooted at `workspace_root`.
/// `slugs` and `web_bases` are per-thread caches so neither the slug lookup nor
/// the `git remote` read repeats every pass.
fn resolve_repos(
    workspace_root: &Path,
    slugs: &mut HashMap<PathBuf, String>,
    web_bases: &mut HashMap<PathBuf, String>,
) -> Vec<RepoInput> {
    let registry = crate::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
    let summary = crate::work_finder::last_tick_summary();
    let host_queue = std::sync::Arc::new(
        summary
            .as_ref()
            .map(|s| s.queue.clone())
            .unwrap_or_default(),
    );
    let cap = summary.as_ref().and_then(|s| s.cap);
    registry
        .effective_roots(workspace_root)
        .into_iter()
        .filter_map(|root| {
            // #9548: the pass writes labels and comments on each repo.
            if !crate::write_scope::gate_root(&root, "star liveness") {
                return None;
            }
            let slug = match slugs.get(&root) {
                Some(s) => s.clone(),
                None => {
                    let s = crate::release_resolve::host::repo_slug(&root)?;
                    slugs.insert(root.clone(), s.clone());
                    s
                }
            };
            let web = web_bases
                .entry(root.clone())
                .or_insert_with(|| web_base(&root))
                .clone();
            let shown = root.display().to_string();
            let tick_rows = summary
                .as_ref()
                .map(|s| {
                    s.queue
                        .iter()
                        .filter(|r| r.repo == shown)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            let pool = pool_for(&root);
            Some(RepoInput {
                root,
                slug,
                tick_rows,
                pool,
                host_queue: host_queue.clone(),
                cap,
                web_base: web,
            })
        })
        .collect()
}

/// Publish one pass's fleet-comms notices on `operator_priority.escalation`
/// (#9321).
///
/// Fire-and-forget by construction: a `None` bus, or a bus with no subscriber
/// attached (safehouse narration disabled), drops the notice and the liveness
/// pass proceeds unaffected. The ask is never *lost* by this — the forge
/// comment carrying the same key was already posted before the notice existed.
fn publish_notices(bus: Option<&crate::event_bus::EventBus>, host: &str, notices: Vec<Notice>) {
    let Some(bus) = bus else { return };
    for notice in notices {
        let kind = if notice.resolved {
            "resolved"
        } else {
            "escalation"
        };
        if let Err(err) = bus.publish(notice.to_event(host)) {
            log::debug!(
                "star_liveness: no subscriber for the {kind} notice on {}#{} ({}): {err}",
                notice.repo,
                notice.issue,
                notice.key
            );
        }
    }
}

/// Run `pass` unless the shared rate-limit breaker is tripped (#10020).
///
/// While suppressed the pass is skipped entirely, so no `gh` call is made and
/// the star-intent queue is not drained (the closure owns the drain); queued
/// intents apply on the next live pass. Logs INFO on the first skip of a
/// suppression episode and DEBUG afterwards. `is_suppressed` is injected so
/// tests never touch the global breaker singleton. Returns whether `pass` ran.
pub(super) fn run_pass_unless_suppressed(
    is_suppressed: impl Fn() -> bool,
    already_logged: &mut bool,
    pass: impl FnOnce(),
) -> bool {
    if is_suppressed() {
        if *already_logged {
            log::debug!("star_liveness: rate-limit breaker still tripped; skipping pass");
        } else {
            log::info!("star_liveness: rate-limit breaker tripped; skipping pass until it clears");
            *already_logged = true;
        }
        return false;
    }
    *already_logged = false;
    pass();
    true
}

/// Start the liveness thread for the daemon rooted at `workspace_root`.
/// Called once, only while the work finder is enabled.
///
/// `bus` (#9321) is the daemon's event bus, on which each pass's new
/// escalations and recoveries are published as
/// `operator_priority.escalation` for the Safehouse sink to relay into the
/// team's Matrix room. `None` degrades to the pre-#9321 behavior: the forge
/// comment and the status/queue surfaces still carry the ask, nothing reaches
/// Matrix.
pub fn spawn(
    workspace_root: PathBuf,
    bus: Option<std::sync::Arc<crate::event_bus::EventBus>>,
) -> Option<std::thread::JoinHandle<()>> {
    let spawned = std::thread::Builder::new()
        .name("star-liveness".to_string())
        .spawn(move || {
            let mut state = LivenessState::default();
            let mut slugs = HashMap::new();
            let mut web_bases = HashMap::new();
            let host = crate::sweep_registry::host_identity();
            let mail = super::mail::sink_from_env(&host);
            let mut skip_logged = false;
            // Let the work finder complete a first tick before the first pass.
            std::thread::sleep(Duration::from_secs(30));
            loop {
                let settings = Settings::resolve(&workspace_root);
                run_pass_unless_suppressed(
                    crate::rate_limit_breaker::global_is_suppressed,
                    &mut skip_logged,
                    || {
                        let repos = resolve_repos(&workspace_root, &mut slugs, &mut web_bases);
                        let batch = intents::global_queue()
                            .map(|q| q.drain())
                            .unwrap_or_default();
                        let mut factory = |root: &Path, slug: &str| -> Box<dyn StarForge> {
                            Box::new(GhStarForge::new(root, slug))
                        };
                        let report = state.run_pass(
                            &repos,
                            batch,
                            settings,
                            &host,
                            Utc::now(),
                            &mut factory,
                        );
                        let notices = state.take_notices();
                        super::mail::dispatch_notices(mail.as_ref(), &notices);
                        publish_notices(bus.as_deref(), &host, notices);
                        super::publish_report(report);
                    },
                );
                std::thread::sleep(settings.interval);
            }
        });
    match spawned {
        Ok(handle) => {
            log::info!("star_liveness: starred-issue liveness check running (#9244)");
            Some(handle)
        }
        Err(e) => {
            log::warn!("star_liveness: could not start the liveness thread: {e}");
            None
        }
    }
}
