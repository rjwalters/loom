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
use super::escalate::{Ledger, Outcome};
use super::forge::{GhStarForge, StarForge};
use super::inherit::{self, Inherited};
use super::intents::{self, AppliedIds, StarIntent};
use super::progress::{self, Tracker, Watched};
use super::Settings;
use crate::types::{DroppedStarIntent, ReadyQueueRow, StarLandingRow, StarLivenessReport};

/// Most dropped intents the report keeps.
const MAX_DROPPED: usize = 50;
/// Most unmanaged stars remembered.
const MAX_UNMANAGED: usize = 200;

/// One managed repo, resolved.
#[derive(Debug, Clone)]
pub struct RepoInput {
    pub root: PathBuf,
    /// Forge `owner/repo`.
    pub slug: String,
    /// The last work-finder tick's rows for this root.
    pub tick_rows: Vec<ReadyQueueRow>,
    /// This host's pool exhaustion for the root's pool: (description,
    /// episode).
    pub pool: Option<(String, String)>,
}

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
}

impl LivenessState {
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
        for repo in repos {
            let root = repo.root.clone();
            let checkpoint = move |n: u32| checkpoint_stamp(&root, n);
            let root2 = repo.root.clone();
            let recorded = move |n: u32| intents::recorded_starred_at(&root2, n);
            let ctx = RepoContext {
                slug: &repo.slug,
                host,
                tick_rows: &repo.tick_rows,
                pool: repo.pool.clone(),
                checkpoint: &checkpoint,
                recorded_starred_at: &recorded,
            };
            let mut forge = forges(&repo.root, &repo.slug);
            let result = collect::Evaluator::new(forge.as_mut(), ctx, &mut self.refusals).run();
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
                    evaluated.extend(rows.into_iter().map(|e| (Some(repo.root.clone()), e)));
                }
                Err(e) => {
                    log::warn!("star_liveness: evaluating {} failed: {e}", repo.slug);
                    failed.push(repo.slug.clone());
                }
            }
        }
        for ((slug, number), at) in &self.unmanaged {
            evaluated.push((None, collect::unmanaged(slug, *number, at.clone())));
        }

        self.ledger.host = host.to_string();
        self.ledger.write = settings.escalate;
        let mut rows = Vec::new();
        let mut live = HashSet::new();
        let mut posted = 0;
        for (root, e) in evaluated {
            let repo = e.facts.repo.clone();
            let issue = e.facts.issue.number;
            live.insert((repo.clone(), issue));
            let obs = self
                .tracker
                .observe(&repo, issue, e.landing.stage, &e.fingerprint, now);
            let mut ask = e.landing.ask.clone();
            if ask.is_none() && root.is_some() {
                ask = progress::watchdog(
                    &Watched {
                        repo: &repo,
                        issue,
                        stage: e.landing.stage,
                        next_actor: &e.landing.next_actor,
                        fingerprint: &e.fingerprint,
                        progress_at: obs.progress_at,
                    },
                    now,
                    settings.no_progress,
                );
            }
            if let (Some(a), Some(root)) = (&ask, &root) {
                let mut forge = forges(root, &repo);
                match self
                    .ledger
                    .escalate(forge.as_mut(), &repo, issue, a, e.inherited_from)
                {
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
                ask,
                inherited_from: e.inherited_from,
                operator_priority_at: e
                    .starred_at
                    .clone()
                    .or_else(|| e.facts.issue.created_at.clone()),
                last_progress_at: Some(obs.progress_at),
            });
        }
        self.tracker.retain(&live, &failed);
        StarLivenessReport {
            at: Some(now),
            rows: order_rows(rows),
            escalations_posted: posted,
            dropped_intents: self.dropped.iter().cloned().collect(),
            failed_repos: failed,
        }
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

/// The checkpoint file's modification time, as a progress stamp.
fn checkpoint_stamp(root: &Path, issue: u32) -> Option<String> {
    let path = root
        .join(".loom")
        .join("sweep-checkpoint")
        .join(format!("issue-{issue}.json"));
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(modified).to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// This host's pool exhaustion for `root`, from the work finder's pool holds.
fn pool_for(root: &Path) -> Option<(String, String)> {
    let dir = crate::tokens_pool::paths::resolve_tokens_dir(root);
    crate::work_finder::pool_preflight::active_hold_statuses()
        .into_iter()
        .find(|h| h.dir == dir)
        .map(|h| {
            (
                format!(
                    "all {} token(s) exhausted since {}, next possible clear ~{}",
                    h.total,
                    h.since.format("%Y-%m-%d %H:%MZ"),
                    h.next_clear_at.format("%H:%MZ")
                ),
                h.since.format("%Y-%m-%dT%H:%M").to_string(),
            )
        })
}

/// Resolve the managed repos for the daemon rooted at `workspace_root`.
fn resolve_repos(workspace_root: &Path, slugs: &mut HashMap<PathBuf, String>) -> Vec<RepoInput> {
    let registry = crate::workspace_registry::WorkspaceRegistry::load_default().unwrap_or_default();
    let summary = crate::work_finder::last_tick_summary();
    registry
        .effective_roots(workspace_root)
        .into_iter()
        .filter_map(|root| {
            let slug = match slugs.get(&root) {
                Some(s) => s.clone(),
                None => {
                    let s = crate::release_resolve::host::repo_slug(&root)?;
                    slugs.insert(root.clone(), s.clone());
                    s
                }
            };
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
            })
        })
        .collect()
}

/// Start the liveness thread for the daemon rooted at `workspace_root`.
/// Called once, only while the work finder is enabled.
pub fn spawn(workspace_root: PathBuf) -> Option<std::thread::JoinHandle<()>> {
    let spawned = std::thread::Builder::new()
        .name("star-liveness".to_string())
        .spawn(move || {
            let mut state = LivenessState::default();
            let mut slugs = HashMap::new();
            let host = crate::sweep_registry::host_identity();
            // Let the work finder complete a first tick before the first pass.
            std::thread::sleep(Duration::from_secs(30));
            loop {
                let settings = Settings::resolve(&workspace_root);
                let repos = resolve_repos(&workspace_root, &mut slugs);
                let batch = intents::global_queue()
                    .map(|q| q.drain())
                    .unwrap_or_default();
                let mut factory = |root: &Path, slug: &str| -> Box<dyn StarForge> {
                    Box::new(GhStarForge::new(root, slug))
                };
                let report =
                    state.run_pass(&repos, batch, settings, &host, Utc::now(), &mut factory);
                super::publish_report(report);
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
