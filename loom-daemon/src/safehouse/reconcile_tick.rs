//! The narration sink's merge-reconciliation tick (#4583), with the #12152
//! changes: a tick the rate-limit breaker holds back is deferred rather than
//! spent, and each workspace keeps a watermark, so the first pass after the
//! breaker clears reads every PR merged since the last answered one.
//!
//! Split out of `safehouse.rs` to keep that file's size flat; it reaches the
//! sink's private helpers through `super::*`.

use super::*;

/// What one reconciliation pass over a workspace found (#12152).
pub(super) struct ReconcilePass {
    /// The completions to narrate, already recorded in the dedup set.
    pub(super) envelopes: Vec<Envelope>,
    /// Merged rows the query returned inside the lookback window.
    pub(super) rows: usize,
    /// The query returned a full page (see [`RECONCILE_CATCHUP_LIMIT`]).
    pub(super) saturated: bool,
}

/// [`reconcile_recent_merges`] with an optional watermark (#12152): with
/// `since`, the query reads every PR merged from then on
/// ([`reconcile_query_args`]). `None` when the forge did not answer, so the
/// caller keeps its watermark and the next pass reads the same window again.
#[allow(clippy::too_many_arguments)]
pub(super) async fn reconcile_merges_since(
    persona: &str,
    workspace_root: &str,
    since: Option<DateTime<Utc>>,
    slug_cache: &mut HashMap<String, RepoIdentity>,
    already_narrated: &mut std::collections::HashSet<(String, u32, u32)>,
    activity_db: Option<&Arc<Mutex<ActivityDb>>>,
    peer_completions: Option<&PeerCompletionHandle>,
) -> Option<ReconcilePass> {
    let (rows, saturated) = fetch_merged_prs_since(Path::new(workspace_root), since).await?;
    let mut pass = ReconcilePass {
        envelopes: Vec::new(),
        rows: rows.len(),
        saturated,
    };
    for row in rows {
        let duration_sec = (row.merged_at - row.created_at).num_seconds().max(0);
        if let Some(envelope) = build_and_narrate_completion(
            persona,
            workspace_root,
            row.issue,
            row.merged,
            duration_sec,
            row.merged_at,
            slug_cache,
            already_narrated,
            activity_db,
            peer_completions,
        )
        .await
        {
            pass.envelopes.push(envelope);
        }
    }
    Some(pass)
}

/// Per-workspace reconciliation watermarks (#12152): when each workspace's
/// last **answered** pass started. The next pass over that workspace asks
/// for everything merged since then (less [`RECONCILE_WATERMARK_SLACK`]),
/// so a stretch where the rate-limit breaker kept the pass out — or a pass
/// whose forge read failed — is read in full on the first pass that gets
/// through, instead of only as far as the newest [`RECONCILE_PR_LIMIT`] PRs
/// reach. Process-lifetime: after a restart the first pass is the #4583
/// unwatermarked query again, whose seven-day lookback and persisted dedup
/// set already cover a daemon outage.
#[derive(Debug, Default)]
pub(super) struct ReconcileWatermarks {
    by_root: HashMap<String, DateTime<Utc>>,
}

impl ReconcileWatermarks {
    /// Where the next query over `root` starts, `None` before its first
    /// answered pass.
    pub(super) fn since(&self, root: &str) -> Option<DateTime<Utc>> {
        self.by_root
            .get(root)
            .map(|at| *at - RECONCILE_WATERMARK_SLACK)
    }

    /// Record an answered pass over `root` that started at `started`. Never
    /// moves a watermark backwards.
    pub(super) fn advance(&mut self, root: &str, started: DateTime<Utc>) {
        let slot = self.by_root.entry(root.to_owned()).or_insert(started);
        if started > *slot {
            *slot = started;
        }
    }
}

/// The reconciliation tick's state across ticks (#4583, #4649, #8997,
/// #12152), so one tick is a function a test can drive
/// ([`Self::tick`]).
#[derive(Debug, Default)]
pub(super) struct ReconcileState {
    /// Round-robins one workspace per tick (see `reconciliation_targets`)
    /// rather than sweeping every known workspace in one tick, so one
    /// slow/offline `gh` cannot delay every other workspace's narration
    /// behind it. Not advanced by a deferred tick.
    pub(super) cursor: usize,
    /// Whether the persisted dedup file was fresh at startup (#4649).
    pub(super) dedup_state_was_fresh: bool,
    /// Workspaces that have had their one-time seed-only pass (#4649).
    pub(super) seeded_fresh_workspaces: std::collections::HashSet<String>,
    /// See [`ReconcileWatermarks`].
    pub(super) watermarks: ReconcileWatermarks,
    /// When the breaker started holding passes back, while it still does:
    /// the info line is written on the edges, not once per deferred tick.
    pub(super) deferred_since: Option<DateTime<Utc>>,
}

/// What one reconciliation tick produced for the sink to send.
pub(super) struct ReconcileTickOutput {
    pub(super) envelopes: Vec<Envelope>,
    /// The workspace whose completions `envelopes` holds (room routing).
    pub(super) workspace_root: Option<String>,
}

impl ReconcileState {
    pub(super) fn new(dedup_state_was_fresh: bool) -> Self {
        Self {
            dedup_state_was_fresh,
            ..Self::default()
        }
    }

    /// One reconciliation tick over the next workspace in `targets`.
    ///
    /// While the rate-limit breaker cools (#8997) the tick is **deferred**,
    /// not spent: the cursor stays, no seed-only slot is used, the
    /// workspace's watermark stays where it was, and the pass is counted
    /// `deferred`. The first tick after the breaker clears reads that
    /// workspace from its watermark, so every merge in the deferred stretch
    /// is narrated then (#12152). Every answered pass logs one `info` line.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn tick(
        &mut self,
        targets: &[String],
        persona: &str,
        slug_cache: &mut HashMap<String, RepoIdentity>,
        completed: &mut std::collections::HashSet<(String, u32, u32)>,
        activity_db: Option<&Arc<Mutex<ActivityDb>>>,
        peer_completions: Option<&PeerCompletionHandle>,
        completions_path: Option<&Path>,
    ) -> ReconcileTickOutput {
        use crate::observability::ops::feed::{self, PassOutcome, Trigger};
        let mut out = ReconcileTickOutput {
            envelopes: Vec::new(),
            workspace_root: None,
        };
        if targets.is_empty() {
            return out;
        }
        let workspace_root = targets[self.cursor % targets.len()].clone();
        if forge_gate::suppressed("pr list (reconcile)", Path::new(&workspace_root)) {
            if self.deferred_since.is_none() {
                log::info!(
                    "safehouse: merge reconciliation deferred — rate-limit breaker cooling; each workspace catches up from its watermark once it clears (#12152)"
                );
                self.deferred_since = Some(Utc::now());
            }
            feed::record_reconcile_pass(PassOutcome::Deferred);
            return out;
        }
        if let Some(since) = self.deferred_since.take() {
            log::info!(
                "safehouse: rate-limit breaker cleared after {}s; merge reconciliation resuming (#12152)",
                (Utc::now() - since).num_seconds().max(0)
            );
        }
        self.cursor = self.cursor.wrapping_add(1);
        // Seed-only first pass (#4649): this workspace's very first
        // reconciliation tick on a host with no reliable persisted dedup
        // state seeds `completed` (and persists it) but drops the resulting
        // envelopes instead of narrating them — otherwise every in-window
        // merge (up to `RECONCILE_PR_LIMIT`) would burst onto the feed at
        // once. `insert` returns `false` on a repeat visit, so this only ever
        // suppresses one tick per workspace.
        let since = self.watermarks.since(&workspace_root);
        let started = Utc::now();
        let Some(pass) = reconcile_merges_since(
            persona,
            &workspace_root,
            since,
            slug_cache,
            completed,
            activity_db,
            peer_completions,
        )
        .await
        else {
            log::info!(
                "safehouse: reconcile pass over {workspace_root} got no forge answer; its watermark holds for the next pass (#12152)"
            );
            feed::record_reconcile_pass(PassOutcome::Failed);
            return out;
        };
        feed::record_reconcile_pass(PassOutcome::Ran);
        self.watermarks.advance(&workspace_root, started);
        let seed_only = self.dedup_state_was_fresh
            && self.seeded_fresh_workspaces.insert(workspace_root.clone());
        let found = pass.envelopes.len();
        if found > 0 {
            persist_completed_best_effort(completions_path, completed);
        }
        feed::record_completions(Trigger::Reconcile, seed_only, found);
        log::info!(
            "safehouse: reconcile pass over {workspace_root}: {} merged row(s) {}, {found} new completion(s){} (#12152)",
            pass.rows,
            since.map_or_else(
                || "in the newest page".to_owned(),
                |t| format!("since {}", t.to_rfc3339_opts(SecondsFormat::Secs, true)),
            ),
            if seed_only && found > 0 {
                " seeded without narrating (fresh dedup file, #4649)"
            } else {
                ""
            }
        );
        if pass.saturated {
            log::warn!(
                "safehouse: reconcile pass over {workspace_root} filled its page ({} rows); merges past it in this window are not narrated (#12152)",
                pass.rows
            );
        }
        if !seed_only {
            out.envelopes = pass.envelopes;
            out.workspace_root = Some(workspace_root);
        }
        out
    }
}
