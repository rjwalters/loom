//! Forge-side pipeline snapshot for `loom-daemon status --pipeline` (Issue
//! #3977).
//!
//! `loom-daemon status` already renders the *dispatch*-side picture (in-flight
//! sweeps, the dynamic concurrency cap, token health, per-repo priority/gate
//! state — see [`crate::types::DaemonStatusReport::per_repo`]). None of that
//! answers "how is the work actually progressing?" — that requires forge
//! queries the daemon's IPC handler deliberately does *not* make (it stays a
//! fast, network-free round-trip). This module is the client-side (CLI-only,
//! opt-in via `--pipeline`) counterpart: for each managed-workspace root it
//! counts open dispatchable `loom:issue` rows (queued — park-labeled rows
//! excluded, see [`RepoPipelineSnapshot::queued`]), open `loom:building`
//! (claimed), open PRs by `loom:review-requested` / `loom:changes-requested`
//! / `loom:pr`, and PRs merged in the last 24h.
//!
//! # Resilience
//!
//! Every count is independently fetched and independently allowed to fail —
//! one unreachable repo (network outage, `gh` not authenticated for that
//! remote, a Gitea-only repo `gh` cannot talk to at all) degrades to `?` for
//! *that* field only and never sinks the rest of the snapshot. This mirrors
//! the work-finder's per-workspace error-handling rule (`tick_multi` in
//! [`crate::work_finder`]): a forge failure aborts only the one workspace's
//! read for that tick, never the whole multi-repo pass.
//!
//! # Parallelism
//!
//! [`collect_pipeline_snapshots`] fans the per-repo fetch out onto Tokio's
//! blocking-thread pool (`gh` is a synchronous subprocess call) so N managed
//! repos cost roughly one repo's worth of wall-clock latency, not N.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// One managed repo's forge-side pipeline counts.
///
/// Every count field is `Option<usize>` — `None` means the underlying forge
/// query failed for *that* metric (rendered as `?`); a `Some` for one field
/// and `None` for another on the same repo is expected under partial
/// degradation (e.g. `gh` rate-limited mid-fetch).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RepoPipelineSnapshot {
    /// The workspace root this line describes (matches
    /// [`crate::types::RepoStatus::root`] for the same repo).
    pub root: PathBuf,
    /// Open issues labeled `loom:issue` — queued, not yet claimed, and
    /// **dispatchable**: rows also carrying a park label
    /// (`crate::work_finder::PARK_LABELS` — `loom:blocked` /
    /// `loom:operator-only`) are excluded, mirroring the work-finder's own
    /// admission check (Issue #4825).
    pub queued: Option<usize>,
    /// Open issues labeled `loom:building` — claimed, in progress.
    pub building: Option<usize>,
    /// Open PRs labeled `loom:review-requested` — awaiting Judge.
    pub review_requested: Option<usize>,
    /// Open PRs labeled `loom:changes-requested` — Doctor's queue.
    pub changes_requested: Option<usize>,
    /// The strict subset of [`Self::changes_requested`] that has **no
    /// owner** (Issue #5272): no active Doctor claim (`loom:treating`) and no
    /// park/hold label (`crate::work_finder::PARK_LABELS` — `loom:blocked` /
    /// `loom:operator-only`). Before #5272's standalone Doctor dispatch, every
    /// row here was a PR permanently parked once its sweep ended — nothing in
    /// the fleet would ever pick it up again. A regression here (this count
    /// climbing and staying nonzero) is exactly the failure mode #5272 fixes,
    /// so it is tracked as its own field rather than folded into
    /// [`Self::changes_requested`], which conflates "Doctor's queue depth"
    /// with "PRs Doctor has actually forgotten about".
    pub changes_requested_unclaimed: Option<usize>,
    /// Open PRs labeled `loom:pr` — Judge-approved, awaiting Champion merge.
    pub approved: Option<usize>,
    /// PRs merged in the last 24h (a throughput signal, not a queue depth).
    pub merged_24h: Option<usize>,
    /// The first forge-query failure encountered for this repo, if any. A
    /// repo can have `Some(error)` and still carry partial `Some(..)` counts
    /// for the metrics that *did* succeed.
    pub error: Option<String>,
}

impl RepoPipelineSnapshot {
    /// Whether every metric for this repo was fetched successfully.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.error.is_none()
    }
}

/// Abstraction over the forge read so the fan-out/aggregation logic
/// ([`collect_pipeline_snapshots`]) is unit-testable without shelling out to
/// `gh` — mirrors [`crate::work_finder::WorkSource`] /
/// [`crate::work_finder::forge::GhWorkSource`].
pub trait PipelineSource {
    /// Fetch the pipeline snapshot for one managed-workspace root. Never
    /// panics or propagates an error — any forge failure is captured in
    /// [`RepoPipelineSnapshot::error`] (and the specific metric left `None`)
    /// so the caller can render `?` for that repo without losing the rest of
    /// the batch.
    fn fetch(&self, root: &Path) -> RepoPipelineSnapshot;
}

/// Minimal `gh issue/pr list --json number` row — only the count matters, so
/// the field itself is unused beyond deserializing one row per open item.
#[derive(Debug, Deserialize)]
struct NumberRow {
    #[allow(dead_code)]
    number: u64,
}

/// Which of the seven counted metrics a [`GhPipelineSource`] fetches (Issue
/// #4761; widened to seven by Issue #5272).
///
/// Each metric costs one `gh` invocation, and they run *sequentially* within a
/// repo, so the mask is what keeps a consumer that needs two of them from
/// paying for all seven. A metric that is masked off is left `None` — a caller
/// that masks a metric off must simply not read it (every existing caller uses
/// [`Self::ALL`], for which `None` keeps its original "this query failed"
/// meaning).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PipelineMetrics {
    /// Open, dispatchable `loom:issue` rows (park-labeled rows excluded —
    /// see [`RepoPipelineSnapshot::queued`]).
    pub queued: bool,
    /// Open issues labeled `loom:building`.
    pub building: bool,
    /// Open PRs labeled `loom:review-requested`.
    pub review_requested: bool,
    /// Open PRs labeled `loom:changes-requested`.
    pub changes_requested: bool,
    /// The unclaimed/unheld subset of `changes_requested` — see
    /// [`RepoPipelineSnapshot::changes_requested_unclaimed`] (Issue #5272).
    pub changes_requested_unclaimed: bool,
    /// Open PRs labeled `loom:pr`.
    pub approved: bool,
    /// PRs merged inside the configured window.
    pub merged: bool,
}

impl PipelineMetrics {
    /// Every metric — the historical (and default) behavior.
    pub const ALL: Self = Self {
        queued: true,
        building: true,
        review_requested: true,
        changes_requested: true,
        changes_requested_unclaimed: true,
        approved: true,
        merged: true,
    };

    /// The metrics `loom-daemon health` needs: queue depth, the review-side
    /// axes (including the #5272 no-owner count), and merge throughput.
    ///
    /// `building` stays masked off — no health section reads it — so this is
    /// six `gh` calls per repo rather than seven. The three review axes were
    /// masked off too until Issue #5021, which is *why* the `queues` section
    /// could not see a Judge outage: the fields it needed were never fetched
    /// on the CLI path. The extra calls are the cost of that visibility, and
    /// they fan out per repo in parallel
    /// ([`collect_pipeline_snapshots`]), so the wall-clock cost is a handful
    /// of extra sequential `gh` calls total, not per repo.
    pub const HEALTH: Self = Self {
        queued: true,
        building: false,
        review_requested: true,
        changes_requested: true,
        changes_requested_unclaimed: true,
        approved: true,
        merged: true,
    };
}

impl Default for PipelineMetrics {
    fn default() -> Self {
        Self::ALL
    }
}

/// The default merge-throughput window — the 24h the `merged_24h` field is
/// named for, and what every pre-#4761 caller gets.
pub const DEFAULT_MERGE_WINDOW_HOURS: i64 = 24;

/// The default `gh` binary this module invokes when nothing overrides it —
/// the single source of truth so [`GhPipelineSource::new`] and
/// [`probe_gh_availability`]'s caller (`loom-daemon health`'s collector,
/// #5061) can never drift into checking a different binary than the one that
/// actually runs the per-repo queries.
pub const DEFAULT_GH_BIN: &str = "gh";

// ============================================================================
// `gh` binary availability (Issue #5061)
// ============================================================================

/// The client-side `gh` binary this process would use for a
/// [`GhPipelineSource`] fan-out could not be found or executed at all — an
/// **environment fact about this process**, not a forge outage.
///
/// Produced once by [`probe_gh_availability`], *before* any per-repo `gh`
/// call, precisely so a caller (`loom-daemon health`, #5061) can report it as
/// a single fact rather than as N independent "forge query FAILED" entries —
/// one per managed repo — which is what happened before this type existed: a
/// missing `gh` on a non-login SSH `PATH` made every repo's query fail
/// identically, and the resulting message read exactly like a forge outage or
/// a dead credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GhUnavailable {
    /// The configured `gh` binary path/name (`gh` unless overridden via
    /// [`GhPipelineSource::with_gh_bin`]).
    pub gh_bin: String,
    /// Operator-facing reason, already naming the specific cause: PATH
    /// resolution failure (the common case — the same failure class as
    /// #4875) vs. an explicit path that does not exist vs. a file found but
    /// not executable.
    pub reason: String,
    /// This process's raw `PATH` value at probe time, when the failure was a
    /// PATH lookup — `None` for an explicit-path failure (nothing to blame
    /// on `PATH`) or a permission failure. Kept out of `reason` because a
    /// summary line should not have to enumerate an arbitrarily long,
    /// host-specific `PATH`; carried here for `--json` diagnostics.
    pub observed_path: Option<String>,
}

/// Probe whether `gh_bin` can be located and invoked at all: a single,
/// bounded `<gh_bin> --version` spawn attempt — no network, no repo context,
/// just "does exec even start". `Ok(())` on any outcome other than the
/// binary failing to *launch*: a `gh` that launches but exits non-zero (an
/// unexpected `--version` failure) is still "available" for this purpose.
/// Only [`std::io::ErrorKind::NotFound`] (not on `PATH`, or an explicit path
/// that does not exist) and [`std::io::ErrorKind::PermissionDenied`] (found
/// but not executable) are treated as unavailable — matching the acceptance
/// criteria's "missing or non-executable `gh`". Any other spawn error (e.g. a
/// transient fork/exec failure) is left for the real per-repo `gh` calls to
/// surface, exactly as before this probe existed — this function's whole
/// point is narrowly distinguishing "the binary itself cannot run" from
/// every other kind of forge-query failure.
pub fn probe_gh_availability(gh_bin: &Path) -> Result<(), GhUnavailable> {
    match Command::new(gh_bin).arg("--version").output() {
        Ok(_) => Ok(()),
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            Err(describe_gh_unavailable(gh_bin, &e))
        }
        Err(_) => Ok(()),
    }
}

/// Build the operator-facing [`GhUnavailable`] for [`probe_gh_availability`]'s
/// failure, naming the specific PATH problem (the same failure class as
/// #4875) when `gh_bin` is a bare name subject to a `PATH` lookup, rather
/// than an explicit path.
fn describe_gh_unavailable(gh_bin: &Path, err: &std::io::Error) -> GhUnavailable {
    if err.kind() == std::io::ErrorKind::PermissionDenied {
        return GhUnavailable {
            gh_bin: gh_bin.display().to_string(),
            reason: format!(
                "`{}` was found but is not executable ({err}) — check its file permissions",
                gh_bin.display()
            ),
            observed_path: None,
        };
    }

    // A path is subject to PATH-lookup exec semantics only when it has no
    // directory component at all (mirrors how `exec`/`Command` itself decides
    // whether to search `PATH`: any path separator anywhere opts out of the
    // search). An explicit path (`./gh`, `/usr/bin/gh`, a `with_gh_bin`
    // override pointing at a specific file) that does not exist is not a
    // `PATH` problem — no separate hint is useful there.
    let is_bare_name = gh_bin.parent().is_none_or(|p| p.as_os_str().is_empty());
    if !is_bare_name {
        return GhUnavailable {
            gh_bin: gh_bin.display().to_string(),
            reason: format!("`{}` does not exist", gh_bin.display()),
            observed_path: None,
        };
    }

    let path = std::env::var("PATH").unwrap_or_default();
    let hint = crate::fleet::path_bootstrap::CANONICAL_PATH_DIRS
        .iter()
        .take(3)
        .copied()
        .collect::<Vec<_>>()
        .join(", ");
    GhUnavailable {
        gh_bin: gh_bin.display().to_string(),
        reason: format!(
            "`{}` not found on PATH — cannot assess queue depth / merge throughput. This looks \
             like a non-login shell (e.g. a bare `ssh host 'cmd'` invocation) missing PATH \
             entries a login shell would add — the same failure class as #4875. Commonly \
             missing: {hint}",
            gh_bin.display()
        ),
        observed_path: Some(path),
    }
}

/// A `gh`-backed [`PipelineSource`]. Runs up to six `gh` invocations per repo
/// (one per requested metric — see [`PipelineMetrics`]) scoped to that repo's
/// own working directory so `gh` auto-detects the remote — same convention as
/// [`crate::work_finder::forge::GhWorkSource::for_root`]. `--limit 500` is
/// generous headroom over the default `gh` page size of 30, which would
/// otherwise silently undercount a busy repo's queue.
pub struct GhPipelineSource {
    gh_bin: PathBuf,
    metrics: PipelineMetrics,
    merge_window: chrono::Duration,
}

impl GhPipelineSource {
    /// Construct a source using `gh` from `PATH`, fetching every metric over
    /// the default 24h merge window.
    #[must_use]
    pub fn new() -> Self {
        Self {
            gh_bin: PathBuf::from(DEFAULT_GH_BIN),
            metrics: PipelineMetrics::ALL,
            merge_window: chrono::Duration::hours(DEFAULT_MERGE_WINDOW_HOURS),
        }
    }

    /// Override the `gh` binary path (for tests / non-standard installs).
    #[must_use]
    pub fn with_gh_bin(mut self, bin: PathBuf) -> Self {
        self.gh_bin = bin;
        self
    }

    /// Restrict which metrics are fetched (Issue #4761). Unrequested metrics
    /// are left `None`.
    #[must_use]
    pub fn with_metrics(mut self, metrics: PipelineMetrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// Override the merge-throughput window (Issue #4761) — the window
    /// [`RepoPipelineSnapshot::merged_24h`] is counted over. A non-positive
    /// window is ignored (the default 24h is kept) rather than producing a
    /// `merged:>=<future>` query that always returns zero.
    #[must_use]
    pub fn with_merge_window(mut self, window: chrono::Duration) -> Self {
        if window > chrono::Duration::zero() {
            self.merge_window = window;
        }
        self
    }

    /// Run `gh <args>` with `current_dir(root)` and return the length of the
    /// returned JSON array — the count for whatever list query `args`
    /// encodes.
    fn count(&self, root: &Path, args: &[&str]) -> Result<usize> {
        let mut cmd = Command::new(&self.gh_bin);
        cmd.args(args).current_dir(root);
        // #5401: a cross-owner managed repo's count query uses its own owner's
        // installation-token `GH_CONFIG_DIR` (no-op for single-owner fleets).
        crate::credential_preflight::apply_gh_config_for_root(&mut cmd, root);
        let out = cmd
            .output()
            .with_context(|| format!("failed to invoke {}", self.gh_bin.display()))?;
        if !out.status.success() {
            return Err(anyhow!(
                "gh {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        let rows: Vec<NumberRow> =
            serde_json::from_slice(&out.stdout).context("parse gh JSON output")?;
        Ok(rows.len())
    }

    /// The `merged:>=<RFC3339>` search qualifier for "merged inside `window`",
    /// computed from `now`. A separate function so tests can pin the clock.
    fn merged_since_query(now: chrono::DateTime<chrono::Utc>, window: chrono::Duration) -> String {
        let since = now - window;
        format!("merged:>={}", since.format("%Y-%m-%dT%H:%M:%SZ"))
    }

    /// The `--search` query for the `queued` metric: open `loom:issue` rows
    /// that are actually **dispatchable** — i.e. excluding every park label
    /// in [`crate::work_finder::PARK_LABELS`] (Issue #4825).
    ///
    /// `gh issue list --label` ANDs its label flags together with no
    /// negation syntax, so a plain `--label loom:issue` count includes
    /// `loom:blocked`/`loom:operator-only` rows the work-finder would never
    /// admit — `queued` and "the work-finder considers this repo to have
    /// ready work" silently diverged. `--search` supports `-label:` negation,
    /// so this is built from `PARK_LABELS` (not a re-listed literal) so the
    /// two definitions can never drift apart again.
    fn queued_search_query() -> String {
        let mut query = "is:open label:loom:issue".to_string();
        for label in crate::work_finder::PARK_LABELS {
            query.push_str(&format!(" -label:{label}"));
        }
        query
    }

    /// The `--search` query for the `changes_requested_unclaimed` metric
    /// (Issue #5272): open `loom:changes-requested` PRs that are **owned by
    /// nothing** — no active Doctor claim (`loom:treating`) and no park/hold
    /// label (the same [`crate::work_finder::PARK_LABELS`] the `queued`
    /// query above excludes, reused rather than re-listed so the two
    /// definitions can never drift apart). Mirrors [`Self::queued_search_query`]'s
    /// `--search`-negation shape for the identical reason: `gh pr list --label`
    /// only ANDs, it cannot express "and NOT this label".
    fn changes_requested_unclaimed_search_query() -> String {
        let mut query =
            "is:open is:pr label:loom:changes-requested -label:loom:treating".to_string();
        for label in crate::work_finder::PARK_LABELS {
            query.push_str(&format!(" -label:{label}"));
        }
        query
    }
}

impl Default for GhPipelineSource {
    fn default() -> Self {
        Self::new()
    }
}

impl PipelineSource for GhPipelineSource {
    fn fetch(&self, root: &Path) -> RepoPipelineSnapshot {
        let mut snap = RepoPipelineSnapshot {
            root: root.to_path_buf(),
            ..Default::default()
        };
        let mut first_err: Option<String> = None;
        let mut record = |result: Result<usize>| -> Option<usize> {
            match result {
                Ok(n) => Some(n),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e.to_string());
                    }
                    None
                }
            }
        };

        if self.metrics.queued {
            let search = Self::queued_search_query();
            snap.queued = record(self.count(
                root,
                &[
                    "issue", "list", "--search", &search, "--json", "number", "--limit", "500",
                ],
            ));
        }
        if self.metrics.building {
            snap.building = record(self.count(
                root,
                &[
                    "issue",
                    "list",
                    "--state",
                    "open",
                    "--label",
                    "loom:building",
                    "--json",
                    "number",
                    "--limit",
                    "500",
                ],
            ));
        }
        if self.metrics.review_requested {
            snap.review_requested = record(self.count(
                root,
                &[
                    "pr",
                    "list",
                    "--state",
                    "open",
                    "--label",
                    "loom:review-requested",
                    "--json",
                    "number",
                    "--limit",
                    "500",
                ],
            ));
        }
        if self.metrics.changes_requested {
            snap.changes_requested = record(self.count(
                root,
                &[
                    "pr",
                    "list",
                    "--state",
                    "open",
                    "--label",
                    "loom:changes-requested",
                    "--json",
                    "number",
                    "--limit",
                    "500",
                ],
            ));
        }
        if self.metrics.changes_requested_unclaimed {
            let search = Self::changes_requested_unclaimed_search_query();
            snap.changes_requested_unclaimed = record(self.count(
                root,
                &[
                    "pr", "list", "--search", &search, "--json", "number", "--limit", "500",
                ],
            ));
        }
        if self.metrics.approved {
            snap.approved = record(self.count(
                root,
                &[
                    "pr", "list", "--state", "open", "--label", "loom:pr", "--json", "number",
                    "--limit", "500",
                ],
            ));
        }

        if self.metrics.merged {
            let search = Self::merged_since_query(chrono::Utc::now(), self.merge_window);
            snap.merged_24h = record(self.count(
                root,
                &[
                    "pr", "list", "--state", "merged", "--search", &search, "--json", "number",
                    "--limit", "500",
                ],
            ));
        }

        snap.error = first_err;
        snap
    }
}

/// Fetch pipeline snapshots for every root in `roots`, in parallel, preserving
/// input order in the output (so a caller rendering alongside
/// `report.per_repo`'s priority order gets a matching row order). Each fetch
/// runs on Tokio's blocking-thread pool since `gh` is a synchronous subprocess
/// call; one root's failure (including a panicking/aborted fetch task) never
/// prevents the others from completing (#3977 AC2).
pub async fn collect_pipeline_snapshots<S>(
    source: Arc<S>,
    roots: Vec<PathBuf>,
) -> Vec<RepoPipelineSnapshot>
where
    // `?Sized` (Issue #4393) lets a caller pass `Arc<dyn PipelineSource + Send
    // + Sync>` directly — the dashboard's `/api/pipeline` route holds its
    // configured source as a trait object so tests can substitute a fake one
    // without a second generic threading through `ServeState`/`run`.
    S: PipelineSource + Send + Sync + ?Sized + 'static,
{
    let handles: Vec<(PathBuf, tokio::task::JoinHandle<RepoPipelineSnapshot>)> = roots
        .into_iter()
        .map(|root| {
            let source = Arc::clone(&source);
            let root_for_task = root.clone();
            (root, tokio::task::spawn_blocking(move || source.fetch(&root_for_task)))
        })
        .collect();

    let mut out = Vec::with_capacity(handles.len());
    for (root, handle) in handles {
        match handle.await {
            Ok(snap) => out.push(snap),
            Err(join_err) => out.push(RepoPipelineSnapshot {
                root,
                error: Some(format!("pipeline fetch task failed: {join_err}")),
                ..Default::default()
            }),
        }
    }
    out
}

/// Render one count for the human table: `?` for a failed metric, else the
/// number.
#[must_use]
pub fn format_count(count: Option<usize>) -> String {
    count.map_or_else(|| "?".to_string(), |n| n.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::sync::Mutex;

    // ===================================================================
    // format_count
    // ===================================================================

    #[test]
    fn format_count_renders_question_mark_for_none() {
        assert_eq!(format_count(None), "?");
    }

    #[test]
    fn format_count_renders_number_for_some() {
        assert_eq!(format_count(Some(7)), "7");
    }

    // ===================================================================
    // probe_gh_availability — #5061
    // ===================================================================

    #[test]
    fn probe_gh_availability_ok_for_a_real_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(tmp.path(), "echo 'gh version 2.0.0'");
        assert_eq!(probe_gh_availability(&gh), Ok(()));
    }

    /// `#[serial]` is load-bearing (#4547): this test mutates the process-wide
    /// `PATH` env var, which every concurrently-running `Command` spawn in
    /// this file's other tests implicitly reads.
    #[test]
    #[serial]
    fn probe_gh_availability_names_the_path_problem_for_a_bare_name_missing_from_path() {
        let saved = std::env::var("PATH").ok();
        std::env::set_var("PATH", "/no-such-loom-test-dir-5061");
        let result = probe_gh_availability(Path::new("definitely-not-a-real-gh-binary-5061"));
        match saved {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }

        let err = result.expect_err("a binary absent from PATH must be reported as unavailable");
        assert_eq!(err.gh_bin, "definitely-not-a-real-gh-binary-5061");
        assert!(err.reason.contains("PATH"), "reason should name PATH: {}", err.reason);
        assert!(
            err.reason.contains("#4875"),
            "reason should cross-reference #4875: {}",
            err.reason
        );
        assert!(
            err.observed_path.is_some(),
            "a PATH-lookup failure should carry the observed PATH for --json"
        );
    }

    #[test]
    fn probe_gh_availability_reports_a_missing_explicit_path_without_a_path_hint() {
        let err = probe_gh_availability(Path::new("/no/such/gh-binary-5061"))
            .expect_err("a nonexistent explicit path must be reported as unavailable");
        assert!(err.reason.contains("does not exist"));
        assert!(
            err.observed_path.is_none(),
            "an explicit-path failure is not a PATH problem, so no PATH should be attached"
        );
    }

    #[cfg(unix)]
    #[test]
    fn probe_gh_availability_reports_permission_denied_for_a_non_executable_file() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("not-executable-gh");
        std::fs::write(&path, "#!/bin/sh\necho hi\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let err = probe_gh_availability(&path)
            .expect_err("a non-executable file must be reported as unavailable");
        assert!(err.reason.contains("not executable"), "reason: {}", err.reason);
        assert!(err.observed_path.is_none());
    }

    // ===================================================================
    // collect_pipeline_snapshots — fan-out/aggregation (no real `gh`)
    // ===================================================================

    /// A scripted [`PipelineSource`]: returns a canned snapshot per root,
    /// keyed by the root's own path so a multi-root test can assert
    /// independent per-repo results.
    struct FakeSource {
        by_root: Mutex<std::collections::HashMap<PathBuf, RepoPipelineSnapshot>>,
    }

    impl FakeSource {
        fn new(entries: Vec<(PathBuf, RepoPipelineSnapshot)>) -> Self {
            Self {
                by_root: Mutex::new(entries.into_iter().collect()),
            }
        }
    }

    impl PipelineSource for FakeSource {
        fn fetch(&self, root: &Path) -> RepoPipelineSnapshot {
            self.by_root
                .lock()
                .unwrap()
                .get(root)
                .cloned()
                .unwrap_or_else(|| RepoPipelineSnapshot {
                    root: root.to_path_buf(),
                    error: Some("no fixture for this root".to_string()),
                    ..Default::default()
                })
        }
    }

    #[tokio::test]
    async fn collect_preserves_input_order() {
        let a = PathBuf::from("/repo/a");
        let b = PathBuf::from("/repo/b");
        let c = PathBuf::from("/repo/c");
        let source = Arc::new(FakeSource::new(vec![
            (
                a.clone(),
                RepoPipelineSnapshot {
                    root: a.clone(),
                    queued: Some(1),
                    ..Default::default()
                },
            ),
            (
                b.clone(),
                RepoPipelineSnapshot {
                    root: b.clone(),
                    queued: Some(2),
                    ..Default::default()
                },
            ),
            (
                c.clone(),
                RepoPipelineSnapshot {
                    root: c.clone(),
                    queued: Some(3),
                    ..Default::default()
                },
            ),
        ]));

        let out = collect_pipeline_snapshots(source, vec![a.clone(), b.clone(), c.clone()]).await;

        assert_eq!(out.len(), 3);
        assert_eq!(out[0].root, a);
        assert_eq!(out[1].root, b);
        assert_eq!(out[2].root, c);
        assert_eq!(out[0].queued, Some(1));
        assert_eq!(out[1].queued, Some(2));
        assert_eq!(out[2].queued, Some(3));
    }

    /// One repo's forge failure must not affect a sibling's snapshot — the
    /// core #3977 AC2 resilience guarantee, exercised at the aggregation
    /// layer (`GhPipelineSource` covers the same guarantee at the
    /// `gh`-invocation layer below).
    #[tokio::test]
    async fn collect_isolates_one_repo_failure_from_others() {
        let healthy = PathBuf::from("/repo/healthy");
        let unreachable = PathBuf::from("/repo/unreachable");
        let source = Arc::new(FakeSource::new(vec![
            (
                healthy.clone(),
                RepoPipelineSnapshot {
                    root: healthy.clone(),
                    queued: Some(5),
                    building: Some(2),
                    ..Default::default()
                },
            ),
            (
                unreachable.clone(),
                RepoPipelineSnapshot {
                    root: unreachable.clone(),
                    error: Some("network unreachable".to_string()),
                    ..Default::default()
                },
            ),
        ]));

        let out =
            collect_pipeline_snapshots(source, vec![healthy.clone(), unreachable.clone()]).await;

        assert!(out[0].is_complete());
        assert_eq!(out[0].queued, Some(5));
        assert!(!out[1].is_complete());
        assert_eq!(out[1].queued, None);
        assert_eq!(format_count(out[1].queued), "?");
    }

    #[tokio::test]
    async fn collect_empty_roots_is_empty() {
        let source = Arc::new(FakeSource::new(vec![]));
        let out = collect_pipeline_snapshots(source, vec![]).await;
        assert!(out.is_empty());
    }

    // ===================================================================
    // GhPipelineSource — real fan-out against a fake `gh` binary
    // ===================================================================

    /// A fake `gh` script that returns a fixed-length JSON array based on
    /// which label/state substring appears in its argv, mirroring the
    /// `write_fake_script` fixture pattern used in
    /// `token_ranking_refresh.rs`'s tests.
    fn write_fake_gh(dir: &Path, body: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join("fake-gh.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    /// `#[serial]` is load-bearing (#4547): concurrent `std::env::set_var`
    /// (used by `disk_headroom.rs`'s PATH-mutating tests) races any
    /// concurrently-running thread that reads the process environment,
    /// including the implicit env read inside every `std::process::Command`
    /// spawn below — not just subprocesses that literally do a `PATH`
    /// lookup for their own executable.
    #[test]
    #[serial]
    fn gh_pipeline_source_counts_each_metric_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(
            tmp.path(),
            r#"
case "$*" in
  *"--search is:open label:loom:issue"*)
    echo '[{"number":1},{"number":2},{"number":3}]'
    ;;
  *"--label loom:building"*)
    echo '[{"number":4}]'
    ;;
  *"--label loom:review-requested"*)
    echo '[{"number":5},{"number":6}]'
    ;;
  *"--label loom:changes-requested"*)
    echo '[]'
    ;;
  *"is:pr label:loom:changes-requested"*)
    echo '[{"number":12}]'
    ;;
  *"--label loom:pr"*)
    echo '[{"number":7}]'
    ;;
  *"--state merged"*)
    echo '[{"number":8},{"number":9},{"number":10},{"number":11}]'
    ;;
  *)
    echo '[]'
    ;;
esac
"#,
        );

        let source = GhPipelineSource::new().with_gh_bin(gh);
        let root = tmp.path();
        let snap = source.fetch(root);

        assert_eq!(snap.root, root);
        assert_eq!(snap.queued, Some(3));
        assert_eq!(snap.building, Some(1));
        assert_eq!(snap.review_requested, Some(2));
        assert_eq!(snap.changes_requested, Some(0));
        assert_eq!(snap.changes_requested_unclaimed, Some(1));
        assert_eq!(snap.approved, Some(1));
        assert_eq!(snap.merged_24h, Some(4));
        assert!(snap.is_complete());
    }

    /// `#[serial]` is load-bearing (#4547): the fake-`gh` script this test
    /// spawns is a subprocess whose resolution depends on the inherited
    /// `PATH`, which `disk_headroom.rs`'s tests mutate process-globally.
    #[test]
    #[serial]
    fn gh_pipeline_source_records_error_but_keeps_other_metrics() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(
            tmp.path(),
            r#"
case "$*" in
  *"--search is:open label:loom:issue"*)
    echo "boom: not authenticated" 1>&2
    exit 1
    ;;
  *"--label loom:building"*)
    echo '[{"number":1}]'
    ;;
  *)
    echo '[]'
    ;;
esac
"#,
        );

        let source = GhPipelineSource::new().with_gh_bin(gh);
        let snap = source.fetch(tmp.path());

        assert_eq!(snap.queued, None);
        assert_eq!(snap.building, Some(1));
        assert!(!snap.is_complete());
        assert!(snap.error.as_deref().unwrap().contains("not authenticated"));
        // Other metrics that had no dedicated case still resolve (as 0),
        // proving one failed call didn't abort the rest of the fetch.
        assert_eq!(snap.merged_24h, Some(0));
    }

    /// `#[serial]` is load-bearing (#4547) for the same reason as
    /// `gh_pipeline_source_counts_each_metric_independently` above — even a
    /// failed `Command::spawn` attempt reads the process environment.
    #[test]
    #[serial]
    fn gh_pipeline_source_missing_binary_is_a_contained_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let source = GhPipelineSource::new().with_gh_bin(PathBuf::from("/no/such/gh-binary"));
        let snap = source.fetch(tmp.path());

        assert!(!snap.is_complete());
        assert_eq!(snap.queued, None);
        assert_eq!(snap.building, None);
        assert_eq!(snap.review_requested, None);
        assert_eq!(snap.changes_requested, None);
        assert_eq!(snap.changes_requested_unclaimed, None);
        assert_eq!(snap.approved, None);
        assert_eq!(snap.merged_24h, None);
    }

    #[test]
    fn merged_since_query_formats_rfc3339_24h_window() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-27T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let query = GhPipelineSource::merged_since_query(now, chrono::Duration::hours(24));
        assert_eq!(query, "merged:>=2026-07-26T12:00:00Z");
    }

    #[test]
    fn merged_since_query_honors_a_custom_window() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-27T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let query = GhPipelineSource::merged_since_query(now, chrono::Duration::minutes(30));
        assert_eq!(query, "merged:>=2026-07-27T11:30:00Z");
    }

    #[test]
    fn a_non_positive_merge_window_keeps_the_default() {
        let source = GhPipelineSource::new().with_merge_window(chrono::Duration::zero());
        assert_eq!(source.merge_window, chrono::Duration::hours(24));
        let source = GhPipelineSource::new().with_merge_window(chrono::Duration::minutes(-5));
        assert_eq!(source.merge_window, chrono::Duration::hours(24));
    }

    /// The #4761 cost control, as widened by #5021 and #5272: the HEALTH mask
    /// must issue exactly the six `gh` calls the health sections read — queue
    /// depth, the four review-side axes (including the #5272 no-owner count),
    /// and merge throughput — and must still skip `building`, which no
    /// section consumes. The mask exists to keep the one-shot command off the
    /// metrics nothing reads, not to be `ALL`.
    #[test]
    #[serial]
    fn health_metrics_mask_fetches_every_axis_the_sections_read_but_not_building() {
        let tmp = tempfile::tempdir().unwrap();
        let calls = tmp.path().join("calls.log");
        let gh = write_fake_gh(
            tmp.path(),
            &format!("echo \"$*\" >> {}\necho '[{{\"number\":1}}]'", calls.display()),
        );

        let source = GhPipelineSource::new()
            .with_gh_bin(gh)
            .with_metrics(PipelineMetrics::HEALTH);
        let snap = source.fetch(tmp.path());

        assert_eq!(snap.queued, Some(1));
        assert_eq!(snap.merged_24h, Some(1));
        assert_eq!(snap.review_requested, Some(1), "#5021: the review axis must be fetched");
        assert_eq!(snap.changes_requested, Some(1));
        assert_eq!(
            snap.changes_requested_unclaimed,
            Some(1),
            "#5272: the no-owner axis must be fetched"
        );
        assert_eq!(snap.approved, Some(1));
        assert_eq!(snap.building, None, "no health section reads `building`");
        assert!(snap.is_complete());

        let log = std::fs::read_to_string(&calls).unwrap();
        assert_eq!(log.lines().count(), 6, "exactly six gh calls, got:\n{log}");
        assert!(log.contains("loom:issue"));
        assert!(log.contains("loom:review-requested"));
        assert!(log.contains("loom:changes-requested"));
        assert!(
            log.contains("-label:loom:treating"),
            "#5272: no-owner query must exclude loom:treating"
        );
        assert!(log.contains("--state merged"));
        assert!(!log.contains("loom:building"));
    }

    #[test]
    fn default_metrics_mask_is_all() {
        assert_eq!(PipelineMetrics::default(), PipelineMetrics::ALL);
    }

    // ===================================================================
    // queued_search_query — #4825
    // ===================================================================

    /// The `queued` query must exclude every label in
    /// `work_finder::PARK_LABELS` so `queued` matches the work-finder's own
    /// definition of dispatchable — the core #4825 fix.
    #[test]
    fn queued_search_query_excludes_every_park_label() {
        let query = GhPipelineSource::queued_search_query();
        assert!(query.contains("is:open"));
        assert!(query.contains("label:loom:issue"));
        for label in crate::work_finder::PARK_LABELS {
            assert!(
                query.contains(&format!("-label:{label}")),
                "expected '-label:{label}' in query: {query}"
            );
        }
    }

    // ===================================================================
    // changes_requested_unclaimed_search_query — #5272
    // ===================================================================

    /// The `changes_requested_unclaimed` query must exclude an active
    /// Doctor claim (`loom:treating`) *and* every park/hold label in
    /// `work_finder::PARK_LABELS` — the two conditions AC2/AC4 of #5272
    /// require for a `loom:changes-requested` PR to count as "no owner".
    #[test]
    fn changes_requested_unclaimed_search_query_excludes_claim_and_park_labels() {
        let query = GhPipelineSource::changes_requested_unclaimed_search_query();
        assert!(query.contains("is:open"));
        assert!(query.contains("is:pr"));
        assert!(query.contains("label:loom:changes-requested"));
        assert!(
            query.contains("-label:loom:treating"),
            "expected '-label:loom:treating' in query: {query}"
        );
        for label in crate::work_finder::PARK_LABELS {
            assert!(
                query.contains(&format!("-label:{label}")),
                "expected '-label:{label}' in query: {query}"
            );
        }
    }

    /// A row that is `loom:issue` *and* park-labeled must not be counted
    /// toward `queued` — exercised end-to-end against a fake `gh` that
    /// mimics `--search`'s label-negation semantics.
    #[test]
    #[serial]
    fn gh_pipeline_source_excludes_park_labeled_rows_from_queued() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(
            tmp.path(),
            r#"
case "$*" in
  *"--search is:open label:loom:issue -label:loom:blocked -label:loom:operator-only"*)
    echo '[{"number":1},{"number":2}]'
    ;;
  *)
    echo '[]'
    ;;
esac
"#,
        );

        let source = GhPipelineSource::new()
            .with_gh_bin(gh)
            .with_metrics(PipelineMetrics::HEALTH);
        let snap = source.fetch(tmp.path());

        assert_eq!(snap.queued, Some(2), "the fake gh only matches the fully-negated query");
        assert!(snap.is_complete());
    }
}
