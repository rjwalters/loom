//! The production side of the `loom:blocked` release pass (#10556): the
//! [`ReleaseForge`] over `gh`, the one-workspace runner shared by the CLI verb
//! and the daemon tick, and the tick's gate.
//!
//! # The tick
//!
//! Driven by its own daemon task, [`super::release_task`] (#10763), once per
//! registered workspace per minute — no longer from the work finder's
//! per-root listing, which never ran on a host whose work finder is off even
//! when that host owned the workspace's role shard. Fail-soft: it never fails
//! the task.
//!
//! Gates, in order — each one that refuses leaves the forge untouched and
//! records its [`Outcome`](super::release_outcome::Outcome) (#10763):
//!
//! 1. `LOOM_RELEASE_STALE_BLOCKED` — **default on**, like `intake_reconcile`
//!    (#10041): `0`/`false`/`off`/`no` disables, `dry-run` plans and logs
//!    without writing. On by default because `label-state-machine.md` already
//!    promises `loom:blocked` clears once its declared blocker resolves, and
//!    the rule here is stricter than Guide's.
//! 2. Served: the work finder or the role runner runs for the workspace on
//!    this host, and the rate-limit breaker is not suppressing forge calls
//!    (both checked by the task).
//! 3. Cadence: `LOOM_RELEASE_STALE_BLOCKED_INTERVAL_SECS` (default 300) per
//!    workspace.
//! 4. Shard ownership: [`crate::role_shard::decide`] — one host of a sharded
//!    fleet acts on a given workspace.
//! 5. Forge-write scope (#9548): [`crate::write_scope::gate_root_with`].
//!
//! `LOOM_RELEASE_STALE_BLOCKED_MAX_WRITES` (default 20) caps the artifacts
//! acted on per pass. One `log::info!` line per pass carries every count.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::batch::GhStaleBlockedForge;
use super::budget::Floor;
use super::release::{run, Config, ReleaseForge, Report};
use super::release_outcome::{classify_gate, classify_report, record};
use crate::comment_trust::TrustPolicy;
use crate::forge_call_stats::ops;
use crate::forge_etag_store as store;
use crate::forge_identity::FleetLogins;
use crate::gh_invocation::{AccessIntent, GhCompletion, GhInvocation, GhTarget, Operation};
use crate::operator_decision::cli::GhForge;
use crate::proc_exec::Completion;

/// Every read here is recorded under this caller in `forge_call_stats`.
const CALLER: &str = "stale_blocked_release";

/// The enable switch.
pub const ENABLE_ENV: &str = "LOOM_RELEASE_STALE_BLOCKED";
/// Seconds between passes per workspace.
pub const INTERVAL_ENV: &str = "LOOM_RELEASE_STALE_BLOCKED_INTERVAL_SECS";
/// Artifacts acted on per pass.
pub const MAX_WRITES_ENV: &str = "LOOM_RELEASE_STALE_BLOCKED_MAX_WRITES";

const DEFAULT_INTERVAL_SECS: u64 = 300;
/// The per-pass write cap's default.
pub const DEFAULT_MAX_WRITES: usize = 20;

const PAGE: usize = 100;
const MAX_PAGES: u32 = 50;

/// How the pass is switched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Off,
    DryRun,
    On,
}

impl Mode {
    /// Parse the [`ENABLE_ENV`] value. Default **on**, like
    /// [`crate::intake_reconcile`]: only an explicit `0`/`false`/`off`/`no`
    /// disables, and `dry-run` plans without writing.
    #[must_use]
    pub fn parse(value: Option<&str>) -> Self {
        match value
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "0" | "false" | "off" | "no" => Self::Off,
            "dry-run" | "dryrun" | "dry_run" => Self::DryRun,
            _ => Self::On,
        }
    }
}

/// Run `pass` only when the switch is on and this host owns the workspace;
/// `pass` receives whether to dry-run. The tick's seam: nothing is read before
/// both answers are yes.
pub fn gated(mode: Mode, owned: bool, pass: impl FnOnce(bool) -> Report) -> Option<Report> {
    if mode == Mode::Off || !owned {
        return None;
    }
    Some(pass(mode == Mode::DryRun))
}

fn env_num<T: std::str::FromStr + PartialOrd + Default>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse::<T>().ok())
        .filter(|n| *n > T::default())
        .unwrap_or(default)
}

static LAST_RUN: LazyLock<Mutex<HashMap<PathBuf, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Cadence gate: true (and records the run) when `root` is due.
fn due(root: &Path, interval_secs: u64) -> bool {
    let mut map = LAST_RUN
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    if let Some(prev) = map.get(root) {
        if now.duration_since(*prev).as_secs() < interval_secs {
            return false;
        }
    }
    map.insert(root.to_path_buf(), now);
    true
}

/// The switch as the environment sets it now.
#[must_use]
pub fn mode() -> Mode {
    Mode::parse(std::env::var(ENABLE_ENV).ok().as_deref())
}

/// The per-root tick, after [`super::release_task`]'s served and rate-limit
/// gates: run the pass for `root` if enabled, due, owned and in write scope.
/// Never fails; returns the report when a pass ran. Every call records exactly
/// one [`super::release_outcome::Outcome`] (#10763).
pub fn maybe_run(gh_bin: &Path, root: &Path) -> Option<Report> {
    // The pass is exercised directly through `release::run` and `gated`.
    if cfg!(test) {
        return None;
    }
    let mode = mode();
    // Each gate is asked only when every earlier one passed: `Off` must not
    // burn the cadence window, and a host not due need not resolve its shard.
    let due_now = mode != Mode::Off && due(root, env_num(INTERVAL_ENV, DEFAULT_INTERVAL_SECS));
    let owned = due_now && crate::role_shard::decide(root).admits_role_tick();
    if let Some(outcome) = classify_gate(mode, due_now, owned) {
        record(root, outcome, None);
        return None;
    }
    let report = gated(mode, owned, |dry_run| {
        if !dry_run && !crate::write_scope::gate_root_with(root, gh_bin, "stale-blocked release") {
            return Report {
                enumerate_error: Some("outside the forge-write scope (#9548)".to_string()),
                ..Report::default()
            };
        }
        run_for_root(
            root,
            None,
            &Config {
                dry_run,
                max_writes: env_num(MAX_WRITES_ENV, DEFAULT_MAX_WRITES),
                floor: Floor::default(),
            },
        )
    })?;
    record(root, classify_report(mode == Mode::DryRun, &report), Some(&report));
    log::info!("stale_blocked_release: {} — {} (#10556)", root.display(), report.summary());
    Some(report)
}

/// One pass over the workspace at `root` (or the explicit `repo`).
#[must_use]
pub fn run_for_root(root: &Path, repo: Option<&str>, cfg: &Config) -> Report {
    let mut gather = GhStaleBlockedForge::new(root, repo);
    let mut park = GhForge::new(root.to_path_buf(), repo.map(str::to_string));
    let mut extra = GhReleaseForge::new(root, repo);
    let mut report = run(
        &mut gather,
        &mut park,
        &mut extra,
        &FleetLogins::for_root(root),
        &TrustPolicy::for_root(root),
        cfg,
    );
    if let Some(why) = extra.zero_row_anomaly(&report) {
        report.enumerate_error = Some(why);
    }
    report
}

/// Minimum seconds between two zero-row cross-checks of one repo.
pub const ANOMALY_CHECK_SECS: u64 = 3600;

static ANOMALY_CHECKED: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// True (and records the check) when `slug` may be cross-checked at `now`.
fn anomaly_check_due(slug: &str, now: Instant) -> bool {
    let mut map = ANOMALY_CHECKED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if map
        .get(slug)
        .is_some_and(|prev| now.duration_since(*prev).as_secs() < ANOMALY_CHECK_SECS)
    {
        return false;
    }
    map.insert(slug.to_string(), now);
    true
}

/// The anomaly text when the cross-check found open rows the listing missed.
#[must_use]
pub fn zero_row_message(cross_check_rows: usize, slug: &str) -> Option<String> {
    (cross_check_rows > 0).then(|| {
        format!(
            "anomaly: loom:blocked listing returned 0 rows but an uncached read of {slug} \
             found open loom:blocked issues (stale ETag cache entry, wrong slug or token \
             scope?) (#10763)"
        )
    })
}

/// Deadline for the zero-row cross-check's one `gh api` call. The task visits
/// roots sequentially, so an unbounded read here would stall every later root.
const CROSS_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// Open `loom:blocked` rows an uncached REST read of `slug` returns (at most
/// one is asked for). Every failure — spawn, timeout, non-zero exit, a body that
/// is not a JSON array — is an `Err`, never a clean empty result.
fn cross_check_rows(
    gh_bin: &Path,
    root: &Path,
    slug: &str,
    timeout: Duration,
) -> Result<usize, String> {
    let url = format!("repos/{slug}/issues?labels=loom:blocked&state=open&per_page=1");
    let out = match GhInvocation::new(
        Operation::new(CALLER),
        AccessIntent::Read,
        GhTarget::None,
        timeout,
    )
    .program(gh_bin)
    .current_dir(root)
    .args(["api", &url])
    .execute()
    {
        Ok(GhCompletion::Captured(Completion::Exited(out))) => out,
        Ok(_) => return Err(format!("timed out after {}s", timeout.as_secs_f32())),
        Err(e) => return Err(format!("could not run gh: {e}")),
    };
    if !out.status.success() {
        return Err(format!(
            "gh api exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice::<Vec<Value>>(&out.stdout)
        .map(|rows| rows.len())
        .map_err(|e| format!("invalid response: {e}"))
}

/// The pass's `enumerate_error` text for a cross-check result: the anomaly for
/// missed rows, an explicit failure when the check itself could not complete,
/// `None` only for a verified empty listing.
fn zero_row_verdict(rows: Result<usize, String>, slug: &str) -> Option<String> {
    match rows {
        Ok(n) => zero_row_message(n, slug),
        Err(why) => Some(format!("zero-row cross-check of {slug} failed: {why} (#10763)")),
    }
}

/// [`ReleaseForge`] over `gh`: REST + ETag reads through
/// [`crate::forge_etag_store`], the comment through
/// [`crate::forge_comment::post_comment`] after a write-scope check.
pub struct GhReleaseForge {
    gh_bin: PathBuf,
    root: PathBuf,
    repo: Option<String>,
    slug: String,
    /// Memoized write-scope verdict: `Err(reason)` refuses every write.
    write_ok: Option<Result<(), String>>,
}

impl GhReleaseForge {
    #[must_use]
    pub fn new(root: &Path, repo: Option<&str>) -> Self {
        let repo = repo.map(str::to_string).or_else(|| {
            std::env::var("LOOM_REPO")
                .ok()
                .filter(|r| !r.trim().is_empty())
        });
        let slug = store::resolve_target(Some(root), repo.as_deref())
            .repo
            .unwrap_or_else(|| "{owner}/{repo}".to_string());
        Self {
            gh_bin: PathBuf::from(crate::gh_invocation::gh_bin()),
            root: root.to_path_buf(),
            repo,
            slug,
            write_ok: None,
        }
    }

    /// #10763: a clean zero-row `loom:blocked` listing is cross-checked
    /// against one uncached REST read (bypassing the ETag cache and the
    /// shared listing path), at most once per [`ANOMALY_CHECK_SECS`] per
    /// repo. Open `loom:blocked` rows the listing missed make the pass an
    /// error rather than a silent empty report.
    fn zero_row_anomaly(&self, report: &Report) -> Option<String> {
        if report.examined != 0 || report.archived || report.enumerate_error.is_some() {
            return None;
        }
        if !anomaly_check_due(&self.slug, Instant::now()) {
            return None;
        }
        zero_row_verdict(
            cross_check_rows(&self.gh_bin, &self.root, &self.slug, CROSS_CHECK_TIMEOUT),
            &self.slug,
        )
    }

    fn get(&self, op: crate::forge_call_stats::ForgeOp, url: &str) -> Result<String, String> {
        store::cached_read(
            store::ConditionalRead::new(CALLER, op),
            &self.gh_bin,
            Some(&self.root),
            self.repo.as_deref(),
            url,
            "stale-release-",
        )
        .map_err(|e| e.to_string())?
        .body
        .ok_or_else(|| format!("gh api {url}: HTTP 404"))
    }

    /// Every page of a REST array endpoint.
    fn pages(
        &self,
        op: crate::forge_call_stats::ForgeOp,
        base: &str,
    ) -> Result<Vec<Value>, String> {
        let mut all = Vec::new();
        for page in 1..=MAX_PAGES {
            let url = format!("{base}?per_page={PAGE}&page={page}");
            let rows: Vec<Value> = serde_json::from_str(self.get(op, &url)?.trim())
                .map_err(|e| format!("parse {url}: {e}"))?;
            let short = rows.len() < PAGE;
            all.extend(rows);
            if short {
                return Ok(all);
            }
        }
        Err(format!("more than {} entries at {base}", PAGE * MAX_PAGES as usize))
    }
}

impl ReleaseForge for GhReleaseForge {
    fn archived(&mut self) -> Result<bool, String> {
        // The shared probe (#10562), also behind `check-stale-blocked` and the
        // role runner's archived-root gate.
        let site = store::ConditionalRead::new(CALLER, ops::REPO_VIEW);
        super::batch::probe_archived(
            site,
            &self.gh_bin,
            &self.root,
            self.repo.as_deref(),
            &self.slug,
            "stale-release-",
        )
        .map(|(archived, _)| archived)
    }

    fn comments(&mut self, number: u64) -> Result<Vec<Value>, String> {
        let base = format!("repos/{}/issues/{number}/comments", self.slug);
        self.pages(ops::COMMENT_LIST, &base)
    }

    fn labeled_events(&mut self, number: u64) -> Result<Vec<String>, String> {
        let base = format!("repos/{}/issues/{number}/events", self.slug);
        Ok(self
            .pages(ops::TIMELINE_READ, &base)?
            .iter()
            .filter(|e| e.get("event").and_then(Value::as_str) == Some("labeled"))
            .filter_map(|e| e.pointer("/label/name").and_then(Value::as_str))
            .map(str::to_string)
            .collect())
    }

    fn post_comment(&mut self, number: u64, is_pr: bool, body: &str) -> Result<(), String> {
        let (root, repo) = (&self.root, self.repo.as_deref());
        self.write_ok
            .get_or_insert_with(|| match crate::write_scope::may_write_from(root, repo) {
                crate::write_scope::Verdict::Allow(_) => Ok(()),
                crate::write_scope::Verdict::Deny(why) => {
                    Err(format!("refusing the write (#9548): {why}"))
                }
            })
            .clone()?;
        crate::forge_comment::post_comment(
            &self.gh_bin,
            Some(&self.root),
            &self.slug,
            number,
            is_pr,
            body,
        )
        .map(|_| ())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod anomaly_tests {
    use super::*;

    #[test]
    fn zero_row_listing_with_open_rows_is_an_anomaly() {
        assert!(zero_row_message(1, "o/r").unwrap().contains("anomaly"));
        assert!(zero_row_message(0, "o/r").is_none());
    }

    #[test]
    fn anomaly_check_is_rate_limited_per_repo() {
        let now = Instant::now();
        assert!(anomaly_check_due("anomaly-test/a", now));
        assert!(!anomaly_check_due("anomaly-test/a", now));
        assert!(anomaly_check_due("anomaly-test/b", now));
    }

    #[cfg(unix)]
    fn fake_gh(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("gh");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn hung_cross_check_times_out_and_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let gh = fake_gh(dir.path(), "sleep 30");
        let t = Instant::now();
        let r = cross_check_rows(&gh, dir.path(), "o/r", Duration::from_millis(500));
        assert!(t.elapsed().as_secs() < 10, "the hung read must not block the visit");
        let why = r.clone().unwrap_err();
        assert!(why.contains("timed out"), "{why}");
        let msg = zero_row_verdict(r, "o/r").unwrap();
        assert!(msg.contains("cross-check of o/r failed"), "{msg}");
    }

    #[cfg(unix)]
    #[test]
    fn cross_check_failures_are_not_a_clean_run() {
        let dir = tempfile::tempdir().unwrap();
        let bad_exit = fake_gh(dir.path(), "echo boom >&2; exit 1");
        assert!(cross_check_rows(&bad_exit, dir.path(), "o/r", Duration::from_secs(5))
            .unwrap_err()
            .contains("boom"));
        let bad_body = fake_gh(dir.path(), "echo not-json");
        assert!(cross_check_rows(&bad_body, dir.path(), "o/r", Duration::from_secs(5))
            .unwrap_err()
            .contains("invalid response"));
        let missing = dir.path().join("nope");
        assert!(cross_check_rows(&missing, dir.path(), "o/r", Duration::from_secs(5)).is_err());
        let empty = fake_gh(dir.path(), "echo '[]'");
        let ok = cross_check_rows(&empty, dir.path(), "o/r", Duration::from_secs(5));
        assert_eq!(ok, Ok(0));
        assert!(zero_row_verdict(ok, "o/r").is_none());
    }
}
