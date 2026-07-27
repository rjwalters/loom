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
//! counts open `loom:issue` (queued), open `loom:building` (claimed), open
//! PRs by `loom:review-requested` / `loom:changes-requested` / `loom:pr`, and
//! PRs merged in the last 24h.
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
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// One managed repo's forge-side pipeline counts.
///
/// Every count field is `Option<usize>` — `None` means the underlying forge
/// query failed for *that* metric (rendered as `?`); a `Some` for one field
/// and `None` for another on the same repo is expected under partial
/// degradation (e.g. `gh` rate-limited mid-fetch).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoPipelineSnapshot {
    /// The workspace root this line describes (matches
    /// [`crate::types::RepoStatus::root`] for the same repo).
    pub root: PathBuf,
    /// Open issues labeled `loom:issue` — queued, not yet claimed.
    pub queued: Option<usize>,
    /// Open issues labeled `loom:building` — claimed, in progress.
    pub building: Option<usize>,
    /// Open PRs labeled `loom:review-requested` — awaiting Judge.
    pub review_requested: Option<usize>,
    /// Open PRs labeled `loom:changes-requested` — Doctor's queue.
    pub changes_requested: Option<usize>,
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

/// A `gh`-backed [`PipelineSource`]. Runs six `gh` invocations per repo (one
/// per counted metric) scoped to that repo's own working directory so `gh`
/// auto-detects the remote — same convention as
/// [`crate::work_finder::forge::GhWorkSource::for_root`]. `--limit 500` is
/// generous headroom over the default `gh` page size of 30, which would
/// otherwise silently undercount a busy repo's queue.
pub struct GhPipelineSource {
    gh_bin: PathBuf,
}

impl GhPipelineSource {
    /// Construct a source using `gh` from `PATH`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            gh_bin: PathBuf::from("gh"),
        }
    }

    /// Override the `gh` binary path (for tests / non-standard installs).
    #[must_use]
    pub fn with_gh_bin(mut self, bin: PathBuf) -> Self {
        self.gh_bin = bin;
        self
    }

    /// Run `gh <args>` with `current_dir(root)` and return the length of the
    /// returned JSON array — the count for whatever list query `args`
    /// encodes.
    fn count(&self, root: &Path, args: &[&str]) -> Result<usize> {
        let mut cmd = Command::new(&self.gh_bin);
        cmd.args(args).current_dir(root);
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

    /// The `merged:>=<RFC3339>` search qualifier for "merged in the last 24h",
    /// computed from `now`. A separate function so tests can pin the clock.
    fn merged_since_query(now: chrono::DateTime<chrono::Utc>) -> String {
        let since = now - chrono::Duration::hours(24);
        format!("merged:>={}", since.format("%Y-%m-%dT%H:%M:%SZ"))
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

        snap.queued = record(self.count(
            root,
            &[
                "issue",
                "list",
                "--state",
                "open",
                "--label",
                "loom:issue",
                "--json",
                "number",
                "--limit",
                "500",
            ],
        ));
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
        snap.approved = record(self.count(
            root,
            &[
                "pr", "list", "--state", "open", "--label", "loom:pr", "--json", "number",
                "--limit", "500",
            ],
        ));

        let search = Self::merged_since_query(chrono::Utc::now());
        snap.merged_24h = record(self.count(
            root,
            &[
                "pr", "list", "--state", "merged", "--search", &search, "--json", "number",
                "--limit", "500",
            ],
        ));

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
    S: PipelineSource + Send + Sync + 'static,
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

    #[test]
    fn gh_pipeline_source_counts_each_metric_independently() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(
            tmp.path(),
            r#"
case "$*" in
  *"--label loom:issue "*)
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
        assert_eq!(snap.approved, Some(1));
        assert_eq!(snap.merged_24h, Some(4));
        assert!(snap.is_complete());
    }

    #[test]
    fn gh_pipeline_source_records_error_but_keeps_other_metrics() {
        let tmp = tempfile::tempdir().unwrap();
        let gh = write_fake_gh(
            tmp.path(),
            r#"
case "$*" in
  *"--label loom:issue "*)
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

    #[test]
    fn gh_pipeline_source_missing_binary_is_a_contained_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let source = GhPipelineSource::new().with_gh_bin(PathBuf::from("/no/such/gh-binary"));
        let snap = source.fetch(tmp.path());

        assert!(!snap.is_complete());
        assert_eq!(snap.queued, None);
        assert_eq!(snap.building, None);
        assert_eq!(snap.review_requested, None);
        assert_eq!(snap.changes_requested, None);
        assert_eq!(snap.approved, None);
        assert_eq!(snap.merged_24h, None);
    }

    #[test]
    fn merged_since_query_formats_rfc3339_24h_window() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-07-27T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        let query = GhPipelineSource::merged_since_query(now);
        assert_eq!(query, "merged:>=2026-07-26T12:00:00Z");
    }
}
