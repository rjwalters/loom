//! `loom-daemon labels migrate` (#11511): move open issues and PRs off the
//! retired labels `defaults/labels.json` lists under `retired` (#11513), then
//! delete the retired labels nothing open carries any more.
//!
//! Retiring a label in Loom only cleans this repo: the additive
//! `sync-labels.sh` never deletes or relabels, so every other managed repo
//! keeps the old names (and the open items carrying them) forever. This is
//! the per-repo (and, with `--all-workspaces`, per-host-fleet) step that
//! catches them up. The mapping is data (`Registry::retired`), not code.
//!
//! Safety, in the order a run meets it:
//!
//! - **Dry run by default.** Without `--apply` the command only reads: no
//!   add, remove or delete reaches the forge.
//! - **Write scope** (#9548): `--apply` is gated by
//!   `loom_daemon::write_scope::may_write_from` before the first forge call
//!   (read or write) for that repo; a refused repo exits 2 untouched.
//! - **Open items only.** Loom never cleans labels off closed items (#2838).
//! - **Add before remove.** Each item gains `replaced_by` before it loses the
//!   old label, so an interrupted run never leaves it with neither; a failed
//!   add skips that item's removal.
//! - **Never create a label.** A `replaced_by` missing on the forge reports
//!   `SKIPPED` and touches none of that entry's items (`sync-labels.sh`
//!   creates it).
//! - **`owned: false` is add-only.** Loom never declared the label, so a
//!   consumer may own it: items gain `replaced_by`, and the label is never
//!   removed from an item nor deleted.
//! - **Never delete a label in use.** After the relabels the open usage is
//!   re-read (every page); the label is deleted only when that re-read
//!   answers "none". A failed or truncated read keeps it: an unanswered "is
//!   it in use?" is not a "no" (the `labels undeclared` rule).
//! - **Rate limits stop the run.** A rate-limit reply (or the breaker
//!   shedding the call) ends the run with exit 4 and a partial report; it is
//!   never retried here. Every step is idempotent, so a rerun converges.
//!
//! Every call is `gh api` (REST) through [`GhInvocation`], so it rides the
//! workspace's forge credential (the App installation token where
//! configured), the shared breaker and the call accounting.
//!
//! Exit: 0 nothing left to do; 3 something remains (a dry run with pending
//! work, a kept or skipped label, a failed write); 4 the label list was
//! unreadable or a rate limit stopped the run; 2 bad arguments or a refused
//! `--apply`. With `--all-workspaces`, the highest per-repo code.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use loom_daemon::cmd_out::{CmdOutcome, Unavailable};
use loom_daemon::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use loom_daemon::label_registry::RetiredLabel;
use serde::Serialize;

/// Items the issues listing returns per page (GitHub's maximum).
pub(crate) const PER_PAGE: usize = 100;
/// Pages read before a listing is treated as unanswerable (10,000 items).
const MAX_PAGES: u32 = 100;

/// Why a forge call did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ForgeError {
    /// A rate-limit reply, or the breaker shed the call: stop the run.
    RateLimited(String),
    /// HTTP 404: the label or item label is already gone.
    NotFound(String),
    Other(String),
}

impl fmt::Display for ForgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ForgeError::RateLimited(m) => write!(f, "rate limited: {m}"),
            ForgeError::NotFound(m) | ForgeError::Other(m) => f.write_str(m),
        }
    }
}

/// An open issue or PR and its current labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    pub number: u64,
    pub labels: Vec<String>,
}

/// The forge reads and writes this command makes.
pub(crate) trait MigrateForge {
    /// Every label name on the repo.
    fn live_labels(&self) -> Result<Vec<String>, ForgeError>;
    /// Page `page` (1-based, [`PER_PAGE`] per page) of the OPEN issues and
    /// PRs carrying `label`.
    fn open_page(&self, label: &str, page: u32) -> Result<Vec<Item>, ForgeError>;
    fn add_label(&self, number: u64, label: &str) -> Result<(), ForgeError>;
    /// Remove `label` from one item. [`ForgeError::NotFound`] means it was
    /// not there.
    fn remove_label(&self, number: u64, label: &str) -> Result<(), ForgeError>;
    /// Delete `label` from the repo.
    fn delete_label(&self, label: &str) -> Result<(), ForgeError>;
}

/// One report line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub(crate) enum Event {
    Relabel {
        item: u64,
        label: String,
        to: Option<String>,
        had_target: bool,
    },
    AddOnly {
        item: u64,
        label: String,
        to: String,
    },
    Failed {
        item: u64,
        label: String,
        why: String,
    },
    Delete {
        label: String,
    },
    Kept {
        label: String,
        why: String,
    },
    Absent {
        label: String,
    },
    Skipped {
        label: String,
        target: String,
    },
    Unowned {
        label: String,
        open: usize,
    },
    /// `--keep-labels` (2am#4345): open items are migrated, the label
    /// definition is kept so closed items keep it.
    Retained {
        label: String,
    },
    Stopped {
        why: String,
    },
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Relabel {
                item,
                label,
                to,
                had_target,
            } => {
                write!(f, "RELABEL #{item} {label} -> {}", to.as_deref().unwrap_or("none"))?;
                if *had_target {
                    write!(f, " (already has {})", to.as_deref().unwrap_or_default())?;
                }
                Ok(())
            }
            Event::AddOnly { item, label, to } => write!(f, "ADD-ONLY #{item} {label} +{to}"),
            Event::Failed { item, label, why } => write!(f, "FAILED #{item} {label}: {why}"),
            Event::Delete { label } => write!(f, "DELETE {label}"),
            Event::Kept { label, why } => write!(f, "KEPT {label} ({why})"),
            Event::Absent { label } => write!(f, "ABSENT {label}"),
            Event::Skipped { label, target } => {
                write!(f, "SKIPPED {label} (target {target} missing; run sync-labels.sh first)")
            }
            Event::Unowned { label, open } => write!(
                f,
                "UNOWNED {label} (add-only: Loom never declared it, so it is never removed \
                 or deleted; open: {open})"
            ),
            Event::Retained { label } => {
                write!(f, "RETAINED {label} (definition kept: --keep-labels; closed items keep it)")
            }
            Event::Stopped { why } => {
                write!(f, "STOPPED ({why}); rerun later, every step is idempotent")
            }
        }
    }
}

/// Counters for the `SUMMARY` line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Summary {
    pub relabeled: usize,
    pub added: usize,
    pub deleted: usize,
    pub kept: usize,
    /// Forge writes this run actually made (always 0 without `--apply`).
    pub writes: usize,
    /// Work still to do: pending dry-run steps, kept/skipped labels, failed
    /// writes. Zero means a rerun would change nothing.
    pub remaining: usize,
}

/// One repo's result.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    pub repo: String,
    pub apply: bool,
    pub events: Vec<Event>,
    pub summary: Summary,
    pub exit: i32,
    /// A rate limit (or a shed call) ended the run early.
    pub rate_limited: bool,
}

impl Report {
    fn new(repo: &str, apply: bool) -> Self {
        Report {
            repo: repo.to_string(),
            apply,
            events: Vec::new(),
            summary: Summary::default(),
            exit: 0,
            rate_limited: false,
        }
    }

    fn finish(mut self) -> Self {
        if self.exit == 0 && self.summary.remaining > 0 {
            self.exit = 3;
        }
        self
    }

    fn render(&self, out: &mut String) {
        let mode = if self.apply {
            "apply"
        } else {
            "dry-run (no forge writes; pass --apply to migrate)"
        };
        out.push_str(&format!("MODE {mode}\n"));
        for e in &self.events {
            out.push_str(&format!("{e}\n"));
        }
        let s = &self.summary;
        out.push_str(&format!(
            "SUMMARY relabeled={} added={} deleted={} kept={} writes={} remaining={}\n",
            s.relabeled, s.added, s.deleted, s.kept, s.writes, s.remaining
        ));
    }
}

/// A rate limit (or shed call) ended the run.
struct Stop;

fn ieq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

/// Every open item carrying `label`, all pages, deduplicated by number.
pub(crate) fn list_open(forge: &dyn MigrateForge, label: &str) -> Result<Vec<Item>, ForgeError> {
    let mut all = BTreeMap::new();
    for page in 1..=MAX_PAGES {
        let batch = forge.open_page(label, page)?;
        let full = batch.len() >= PER_PAGE;
        for it in batch {
            all.insert(it.number, it);
        }
        if !full {
            return Ok(all.into_values().collect());
        }
    }
    Err(ForgeError::Other(format!(
        "more than {} open items; listing truncated",
        MAX_PAGES as usize * PER_PAGE
    )))
}

/// `--keep-labels` (2am#4345, operator 2026-10-13: "we don't need to change
/// labels on anything closed"): migrate open items, never delete a retired
/// label's definition. GitHub strips a deleted label from every closed item
/// that carried it, which can't be undone. A process-wide switch set once by
/// [`dispatch`], read only by [`migrate_one`].
static KEEP_LABELS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn set_keep_labels(keep: bool) {
    KEEP_LABELS.store(keep, std::sync::atomic::Ordering::Relaxed);
}

fn keep_labels() -> bool {
    KEEP_LABELS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Migrate every `retired` entry, in order, on one repo.
pub(crate) fn run(
    forge: &dyn MigrateForge,
    repo: &str,
    retired: &[RetiredLabel],
    apply: bool,
) -> Report {
    let mut r = Report::new(repo, apply);
    let live = match forge.live_labels() {
        Ok(l) => l,
        Err(e) => {
            if stop_on_limit(&mut r, &e).is_ok() {
                let why = format!("could not list the repo's labels: {e}");
                r.events.push(Event::Stopped { why });
            }
            r.exit = 4;
            return r;
        }
    };
    // (item, lowercased label) adds this run made or would make, so two
    // retired labels sharing a target add it once, in a dry run too.
    let mut added = BTreeSet::new();
    for entry in retired {
        if migrate_one(forge, entry, apply, &live, &mut added, &mut r).is_err() {
            r.exit = 4;
            return r;
        }
    }
    r.finish()
}

/// Route a rate-limited error to [`Stop`] after recording why.
fn stop_on_limit(r: &mut Report, e: &ForgeError) -> Result<(), Stop> {
    if let ForgeError::RateLimited(_) = e {
        r.events.push(Event::Stopped { why: e.to_string() });
        r.rate_limited = true;
        return Err(Stop);
    }
    Ok(())
}

fn migrate_one(
    forge: &dyn MigrateForge,
    entry: &RetiredLabel,
    apply: bool,
    live: &[String],
    added: &mut BTreeSet<(u64, String)>,
    r: &mut Report,
) -> Result<(), Stop> {
    let name = &entry.name;
    let has = |n: &str| live.iter().any(|l| ieq(l, n));
    if !has(name) {
        r.events.push(Event::Absent {
            label: name.clone(),
        });
        return Ok(());
    }
    if let Some(to) = entry.replaced_by.as_deref().filter(|t| !has(t)) {
        r.events.push(Event::Skipped {
            label: name.clone(),
            target: to.to_string(),
        });
        r.summary.remaining += 1;
        return Ok(());
    }
    let items = match list_open(forge, name) {
        Ok(i) => i,
        Err(e) => {
            stop_on_limit(r, &e)?;
            r.events.push(Event::Kept {
                label: name.clone(),
                why: format!("usage lookup failed: {e}"),
            });
            r.summary.kept += 1;
            r.summary.remaining += 1;
            return Ok(());
        }
    };
    for item in &items {
        relabel_item(forge, entry, item, apply, added, r)?;
    }
    if !entry.owned {
        r.events.push(Event::Unowned {
            label: name.clone(),
            open: items.len(),
        });
        return Ok(());
    }
    if keep_labels() {
        // Open items are done (or counted as remaining above); the
        // definition stays, so nothing is left for this label.
        r.events.push(Event::Retained {
            label: name.clone(),
        });
        return Ok(());
    }
    if !apply {
        r.events.push(Event::Delete {
            label: name.clone(),
        });
        r.summary.remaining += 1;
        return Ok(());
    }
    prune(forge, name, r)
}

/// Add the replacement (if missing), then, for an owned label, remove the old one.
fn relabel_item(
    forge: &dyn MigrateForge,
    entry: &RetiredLabel,
    item: &Item,
    apply: bool,
    added: &mut BTreeSet<(u64, String)>,
    r: &mut Report,
) -> Result<(), Stop> {
    let name = &entry.name;
    let mut had_target = true;
    if let Some(to) = &entry.replaced_by {
        let key = (item.number, to.to_ascii_lowercase());
        had_target = item.labels.iter().any(|l| ieq(l, to)) || added.contains(&key);
        if !had_target {
            if apply {
                if let Err(e) = forge.add_label(item.number, to) {
                    stop_on_limit(r, &e)?;
                    let why = format!("add {to} failed, old label left in place: {e}");
                    r.events.push(Event::Failed {
                        item: item.number,
                        label: name.clone(),
                        why,
                    });
                    r.summary.remaining += 1;
                    return Ok(());
                }
                r.summary.writes += 1;
            } else {
                r.summary.remaining += 1;
            }
            added.insert(key);
            if !entry.owned {
                r.events.push(Event::AddOnly {
                    item: item.number,
                    label: name.clone(),
                    to: to.clone(),
                });
                r.summary.added += 1;
            }
        }
    }
    if !entry.owned {
        return Ok(());
    }
    if apply {
        match forge.remove_label(item.number, name) {
            Ok(()) => r.summary.writes += 1,
            Err(ForgeError::NotFound(_)) => {}
            Err(e) => {
                stop_on_limit(r, &e)?;
                let why = format!("remove failed: {e}");
                r.events.push(Event::Failed {
                    item: item.number,
                    label: name.clone(),
                    why,
                });
                r.summary.remaining += 1;
                return Ok(());
            }
        }
    } else {
        r.summary.remaining += 1;
    }
    r.events.push(Event::Relabel {
        item: item.number,
        label: name.clone(),
        to: entry.replaced_by.clone(),
        had_target: entry.replaced_by.is_some() && had_target,
    });
    r.summary.relabeled += 1;
    Ok(())
}

/// Re-read open usage after the relabels; delete only on a definite "none".
fn prune(forge: &dyn MigrateForge, name: &str, r: &mut Report) -> Result<(), Stop> {
    let why = match list_open(forge, name) {
        Err(e) => {
            stop_on_limit(r, &e)?;
            format!("usage re-check failed: {e}")
        }
        Ok(open) if !open.is_empty() => {
            let list: Vec<String> = open.iter().map(|i| format!("#{}", i.number)).collect();
            format!("open: {}", list.join(" "))
        }
        Ok(_) => match forge.delete_label(name) {
            res @ (Ok(()) | Err(ForgeError::NotFound(_))) => {
                // A 404 is a delete someone else already made: not our write.
                r.summary.writes += usize::from(res.is_ok());
                r.summary.deleted += 1;
                r.events.push(Event::Delete {
                    label: name.to_string(),
                });
                return Ok(());
            }
            Err(e) => {
                stop_on_limit(r, &e)?;
                format!("delete failed: {e}")
            }
        },
    };
    r.events.push(Event::Kept {
        label: name.to_string(),
        why,
    });
    r.summary.kept += 1;
    r.summary.remaining += 1;
    Ok(())
}

/// One repo to migrate and the checkout its write-scope gate is asked from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Target {
    pub repo: String,
    pub root: PathBuf,
}

/// Workspace roots to deduplicated targets (by repo, case-insensitively,
/// first root wins), plus the roots with no GitHub `origin`.
pub(crate) fn dedupe_targets(roots: Vec<(PathBuf, Option<String>)>) -> (Vec<Target>, Vec<PathBuf>) {
    let mut seen = BTreeSet::new();
    let (mut targets, mut unresolved) = (Vec::new(), Vec::new());
    for (root, slug) in roots {
        match slug {
            Some(repo) if seen.insert(repo.to_ascii_lowercase()) => {
                targets.push(Target { repo, root })
            }
            Some(_) => {}
            None => unresolved.push(root),
        }
    }
    (targets, unresolved)
}

/// Run every target in order. With `apply`, a target the `gate` refuses
/// gets exit 2 and no forge call at all. A rate-limit stop ends the fleet
/// run: every repo shares the identity whose budget just ran out.
pub(crate) fn run_targets(
    targets: &[Target],
    retired: &[RetiredLabel],
    apply: bool,
    forge_for: &dyn Fn(&str) -> Box<dyn MigrateForge>,
    gate: &dyn Fn(&Target) -> Result<(), String>,
) -> Vec<Report> {
    let mut reports = Vec::new();
    for t in targets {
        if apply {
            if let Err(why) = gate(t) {
                let mut r = Report::new(&t.repo, apply);
                r.events.push(Event::Stopped {
                    why: format!("refusing --apply (#9548): {why}"),
                });
                r.exit = 2;
                reports.push(r);
                continue;
            }
        }
        let r = run(forge_for(&t.repo).as_ref(), &t.repo, retired, apply);
        let rate_limited = r.rate_limited;
        reports.push(r);
        if rate_limited {
            break;
        }
    }
    reports
}

/// [`MigrateForge`] over `gh api` (REST), scoped to one repo.
pub(crate) struct GhMigrateForge {
    pub repo: String,
}

const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// Minimum spacing between this process's forge WRITES (2am#4345): a bulk
/// triage of ~450 label writes in one hour from the operator account tripped
/// GitHub's secondary write limit, which the whole fleet shares. GitHub's
/// guidance is under 80 content-creating requests a minute and 500 an hour;
/// the default 7.5 s keeps a run under 480 an hour. `LOOM_LABEL_WRITE_INTERVAL_MS`
/// overrides it (0 disables pacing, e.g. for a handful of writes).
const DEFAULT_WRITE_INTERVAL: Duration = Duration::from_millis(7500);

/// The configured write interval.
pub(crate) fn write_interval() -> Duration {
    std::env::var("LOOM_LABEL_WRITE_INTERVAL_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_WRITE_INTERVAL, Duration::from_millis)
}

/// How long to wait before the next write, given the last one's time.
pub(crate) fn pace_delay(
    last: Option<std::time::Instant>,
    now: std::time::Instant,
    interval: Duration,
) -> Duration {
    last.map_or(Duration::ZERO, |t| interval.saturating_sub(now.saturating_duration_since(t)))
}

/// Block until the next write may go, process-wide (every target in an
/// `--all-workspaces` run shares one pace: they share one write budget).
pub(crate) fn pace_write() {
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let mut last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let wait = pace_delay(*last, std::time::Instant::now(), write_interval());
    if !wait.is_zero() {
        std::thread::sleep(wait);
    }
    *last = Some(std::time::Instant::now());
}

/// Percent-encode one URL path/query component.
pub(crate) fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Classify a failed gh call's text.
pub(crate) fn classify(why: String) -> ForgeError {
    let lower = why.to_ascii_lowercase();
    if loom_daemon::rate_limit_breaker::indicates_rate_limit(&why) || lower.contains("http 429") {
        ForgeError::RateLimited(why)
    } else if lower.contains("http 404") {
        ForgeError::NotFound(why)
    } else {
        ForgeError::Other(why)
    }
}

impl GhMigrateForge {
    fn gh(&self, intent: AccessIntent, args: &[&str]) -> Result<Vec<u8>, ForgeError> {
        let target = GhTarget::repo(&self.repo).map_err(ForgeError::Other)?;
        let what = format!("gh {}", args.get(..3).unwrap_or(args).join(" "));
        let r = GhInvocation::new(Operation::new("labels.migrate"), intent, target, GH_TIMEOUT)
            .args(args)
            .run();
        if let Some(o) = r.ok_output() {
            return Ok(o.stdout.clone());
        }
        if let CmdOutcome::Unavailable(Unavailable::Shed { .. }) = r {
            return Err(ForgeError::RateLimited(r.failure_reason(&what)));
        }
        Err(classify(r.failure_reason(&what)))
    }

    fn write(&self, args: &[&str]) -> Result<(), ForgeError> {
        pace_write();
        self.gh(AccessIntent::Write, args).map(|_| ())
    }
}

/// One `[number, [label, ...]]` JSON line per item.
pub(crate) fn parse_items(stdout: &str) -> Result<Vec<Item>, ForgeError> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<(u64, Vec<String>)>(l)
                .map(|(number, labels)| Item { number, labels })
                .map_err(|e| ForgeError::Other(format!("bad item line {l:?}: {e}")))
        })
        .collect()
}

impl MigrateForge for GhMigrateForge {
    fn live_labels(&self) -> Result<Vec<String>, ForgeError> {
        let path = format!("repos/{}/labels?per_page=100", self.repo);
        let out = self.gh(AccessIntent::Read, &["api", "--paginate", &path, "--jq", ".[].name"])?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect())
    }

    fn open_page(&self, label: &str, page: u32) -> Result<Vec<Item>, ForgeError> {
        let path = format!(
            "repos/{}/issues?state=open&labels={}&per_page={PER_PAGE}&page={page}",
            self.repo,
            enc(label)
        );
        let jq = ".[] | [.number, [.labels[].name]] | @json";
        let out = self.gh(AccessIntent::Read, &["api", &path, "--jq", jq])?;
        parse_items(&String::from_utf8_lossy(&out))
    }

    fn add_label(&self, number: u64, label: &str) -> Result<(), ForgeError> {
        let path = format!("repos/{}/issues/{number}/labels", self.repo);
        let field = format!("labels[]={label}");
        self.write(&["api", "-X", "POST", &path, "-f", &field, "--silent"])
    }

    fn remove_label(&self, number: u64, label: &str) -> Result<(), ForgeError> {
        let path = format!("repos/{}/issues/{number}/labels/{}", self.repo, enc(label));
        self.write(&["api", "-X", "DELETE", &path, "--silent"])
    }

    fn delete_label(&self, label: &str) -> Result<(), ForgeError> {
        let path = format!("repos/{}/labels/{}", self.repo, enc(label));
        self.write(&["api", "-X", "DELETE", &path, "--silent"])
    }
}

/// Where `labels migrate` points.
pub(crate) enum Scope {
    Repo(String),
    AllWorkspaces,
    /// The current checkout's `origin`.
    Here(PathBuf),
}

pub(crate) fn resolve_targets(scope: Scope) -> Result<(Vec<Target>, Vec<PathBuf>), String> {
    let cwd = std::env::current_dir().map_err(|e| format!("no working directory: {e}"))?;
    match scope {
        Scope::Repo(repo) => {
            GhTarget::repo(&repo).map_err(|_| "--repo must be OWNER/NAME".to_string())?;
            Ok((vec![Target { repo, root: cwd }], Vec::new()))
        }
        Scope::Here(root) => match loom_daemon::release_resolve::host::repo_slug(&root) {
            Some(repo) => Ok((vec![Target { repo, root }], Vec::new())),
            None => Err(format!(
                "{} has no GitHub origin; pass --repo OWNER/NAME or --all-workspaces",
                root.display()
            )),
        },
        Scope::AllWorkspaces => {
            let reg = loom_daemon::workspace_registry::WorkspaceRegistry::load_default()
                .map_err(|e| format!("cannot read the workspace registry: {e}"))?;
            let roots = reg
                .workspaces
                .into_iter()
                .map(|w| {
                    let slug = loom_daemon::release_resolve::host::repo_slug(&w.root);
                    (w.root, slug)
                })
                .collect();
            Ok(dedupe_targets(roots))
        }
    }
}

pub(crate) fn write_gate(t: &Target) -> Result<(), String> {
    match loom_daemon::write_scope::may_write_from(Path::new(&t.root), Some(&t.repo)) {
        loom_daemon::write_scope::Verdict::Allow(_) => Ok(()),
        loom_daemon::write_scope::Verdict::Deny(why) => Err(why),
    }
}

/// `labels migrate --holds [--repo R | --all-workspaces] [--apply] [--json]`
/// (#11861): see [`super::labels_migrate_holds`].
pub(crate) fn dispatch_holds(scope: Scope, apply: bool, json: bool) -> i32 {
    let fleet = matches!(scope, Scope::AllWorkspaces);
    let (targets, unresolved) = match resolve_targets(scope) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("labels migrate --holds: {e}");
            return 2;
        }
    };
    for root in &unresolved {
        eprintln!("labels migrate --holds: UNRESOLVED {} (no GitHub origin)", root.display());
    }
    let forge_for = |t: &Target| -> Box<dyn super::labels_migrate_holds::HoldsForge> {
        Box::new(super::labels_migrate_holds::GhHoldsForge {
            repo: t.repo.clone(),
            root: t.root.clone(),
        })
    };
    let reports =
        super::labels_migrate_holds::run_targets(&targets, apply, &forge_for, &write_gate);
    super::labels_migrate_holds::finish(&reports, apply, fleet, json)
}

/// `labels migrate [--repo R | --all-workspaces] [--apply] [--json]`.
pub(crate) fn dispatch(
    retired: &[RetiredLabel],
    scope: Scope,
    apply: bool,
    keep: bool,
    json: bool,
) -> i32 {
    set_keep_labels(keep);
    let fleet = matches!(scope, Scope::AllWorkspaces);
    let (targets, unresolved) = match resolve_targets(scope) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("labels migrate: {e}");
            return 2;
        }
    };
    let forge_for = |repo: &str| -> Box<dyn MigrateForge> {
        Box::new(GhMigrateForge {
            repo: repo.to_string(),
        })
    };
    let reports = run_targets(&targets, retired, apply, &forge_for, &write_gate);
    let exit = reports.iter().map(|r| r.exit).max().unwrap_or(0);
    if json {
        let v = serde_json::json!({
            "apply": apply,
            "repos": reports,
            "unresolved_workspaces": unresolved,
            "exit": exit,
        });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return exit;
    }
    let mut out = String::new();
    for r in &reports {
        if fleet {
            out.push_str(&format!("== {} ==\n", r.repo));
        }
        r.render(&mut out);
    }
    if fleet {
        for root in &unresolved {
            out.push_str(&format!("UNRESOLVED {} (no GitHub origin)\n", root.display()));
        }
        let pending = reports.iter().filter(|r| r.exit != 0).count();
        out.push_str(&format!(
            "FLEET repos={} not-done={pending} skipped-workspaces={}\n",
            reports.len(),
            unresolved.len()
        ));
    }
    print!("{out}");
    if exit == 3 && !apply {
        eprintln!("labels migrate: dry run only; rerun with --apply to migrate");
    }
    exit
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "labels_migrate_tests.rs"]
mod tests;
