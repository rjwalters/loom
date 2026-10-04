//! Curator intake reconcile pass (#10041).
//!
//! Curator curates the `loom:triage` queue. Issues filed without any `loom:*`
//! label (by humans or other tools) never entered it, so they starved. This
//! pass makes `loom:triage` the single intake state: on a cadence it lists a
//! workspace's open issues and adds `loom:triage` to every issue carrying no
//! `loom:*` label at all (non-`loom:` labels such as `bug` do not count).
//! Pull requests are never touched.
//!
//! Called from the work finder's per-root listing, i.e. only after the
//! rate-limit breaker has passed. Idempotent: a labeled issue drops out of the
//! next listing, and an already-lifecycle-labeled issue is never selected. Uses
//! REST (`gh api`) only, batch-capped per pass, and never fails the tick.
//!
//! Config (env only; default ON): `LOOM_INTAKE_RECONCILE=0|false|off` disables,
//! `LOOM_INTAKE_RECONCILE_INTERVAL_SECS` (default 300),
//! `LOOM_INTAKE_RECONCILE_MAX_PER_PASS` (default 50).

use crate::claim_reconciliation::gh_call;
use chrono::{DateTime, Duration, Utc};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

/// The intake label applied.
pub const TRIAGE_LABEL: &str = "loom:triage";
const DEFAULT_INTERVAL_SECS: u64 = 300;
const DEFAULT_MAX_PER_PASS: usize = 50;
/// A filer commonly creates then labels; leave brand-new issues alone.
const MIN_AGE_SECS: i64 = 120;

/// One open issue row as listed by the pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntakeRow {
    pub number: u32,
    pub created_at: Option<DateTime<Utc>>,
    pub labels: Vec<String>,
}

/// Issue numbers that need `loom:triage`: no `loom:*` label, old enough, oldest
/// first, at most `cap`.
#[must_use]
pub fn select_unlabeled(rows: &[IntakeRow], now: DateTime<Utc>, cap: usize) -> Vec<u32> {
    let mut picked: Vec<&IntakeRow> = rows
        .iter()
        .filter(|r| !r.labels.iter().any(|l| l.starts_with("loom:")))
        .filter(|r| {
            r.created_at
                .is_none_or(|c| now - c >= Duration::seconds(MIN_AGE_SECS))
        })
        .collect();
    picked.sort_by_key(|r| (r.created_at, r.number));
    picked.into_iter().take(cap).map(|r| r.number).collect()
}

/// Parse `number<TAB>created_at<TAB>label,label` lines.
#[must_use]
pub fn parse_rows(stdout: &str) -> Vec<IntakeRow> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            let number = parts.next()?.trim().parse().ok()?;
            let created_at = parts
                .next()
                .and_then(|s| DateTime::parse_from_rfc3339(s.trim()).ok())
                .map(|d| d.with_timezone(&Utc));
            let labels = parts
                .next()
                .unwrap_or("")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect();
            Some(IntakeRow {
                number,
                created_at,
                labels,
            })
        })
        .collect()
}

fn enabled() -> bool {
    !matches!(
        std::env::var("LOOM_INTAKE_RECONCILE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "off" | "no"
    )
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

/// Run the pass for `root` if enabled and due. Returns issues labeled.
pub fn maybe_run(gh_bin: &Path, root: &Path) -> usize {
    // Unit tests of other modules drive GhWorkSource with real/fake `gh`; the
    // pass is exercised directly via `run_once` instead.
    if cfg!(test)
        || !enabled()
        || !due(root, env_num("LOOM_INTAKE_RECONCILE_INTERVAL_SECS", DEFAULT_INTERVAL_SECS))
    {
        return 0;
    }
    // Forge-write scope (#9548): never label issues in a repo this checkout
    // may not write to.
    if !crate::write_scope::gate_root_with(root, gh_bin, "intake reconcile") {
        return 0;
    }
    run_once(
        gh_bin,
        root,
        Utc::now(),
        env_num("LOOM_INTAKE_RECONCILE_MAX_PER_PASS", DEFAULT_MAX_PER_PASS),
    )
}

/// One ungated pass (tests call this directly).
pub fn run_once(gh_bin: &Path, root: &Path, now: DateTime<Utc>, cap: usize) -> usize {
    const JQ: &str = r#".[] | select(.pull_request|not) | [.number, .created_at, ([.labels[].name]|join(","))] | @tsv"#;
    let listing = gh_call::ok_stdout(gh_call::read("intake.list_open", gh_bin, root).args([
        "api",
        "--paginate",
        "repos/{owner}/{repo}/issues?state=open&per_page=100",
        "--jq",
        JQ,
    ]));
    let Some(stdout) = listing else {
        log::warn!("intake_reconcile: open-issue listing failed in {}", root.display());
        return 0;
    };
    let rows = parse_rows(&String::from_utf8_lossy(&stdout));
    let mut labeled = 0;
    for n in select_unlabeled(&rows, now, cap) {
        let path = format!("repos/{{owner}}/{{repo}}/issues/{n}/labels");
        let label = format!("labels[]={TRIAGE_LABEL}");
        let ok = gh_call::output(
            gh_call::write("intake.add_triage", gh_bin, root)
                .args(["api", "-X", "POST", &path, "-f", &label]),
        )
        .is_ok_and(|o| o.status.success());
        if ok {
            labeled += 1;
        } else {
            log::warn!("intake_reconcile: failed to label #{n} in {}", root.display());
            break; // likely rate limited / auth; retry next pass
        }
    }
    if labeled > 0 {
        log::info!(
            "intake_reconcile: applied {TRIAGE_LABEL} to {labeled} unlabeled issue(s) (#10041)"
        );
    }
    labeled
}

#[cfg(test)]
mod tests;
