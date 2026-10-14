//! `loom-daemon labels migrate --holds` (#11861): the one-time pass that
//! takes `loom:operator` off every item a role, not a human, put it on.
//!
//! Since #11861 `loom:operator` is a no-op for every role: only a human
//! removes it. Two kinds of open item carry it for an automated reason, and
//! each moves to `loom:blocked`, which the automation re-evaluates:
//!
//! 1. **Champion-held pull requests.** A PR whose trusted comments carry a
//!    standing merge-risk or critical-file hold marker
//!    ([`loom_daemon::champion_hold::standing_hold`]) gains a reason-only park
//!    record and `loom:blocked`, then loses the operator hold under every
//!    name. **`loom:pr` is never touched**: `loom:pr` + `loom:blocked` with a
//!    standing hold marker is the one shape the verdict-label invariant
//!    allows (the janitor exception), so the Judge's approval survives and a
//!    released hold merges without a fresh review.
//! 2. **Dependency-only escalations.** A proposal (`loom:curated`,
//!    `loom:architect`, `loom:hermit`, `loom:auditor`) Champion escalated for
//!    a timing finding is handed to
//!    [`loom_daemon::dep_classify::cli::migrate_escalation`]: blockers closed
//!    → the hold comes off (Pass 0's un-escalation, one last time); blockers
//!    still open → `loom:blocked` + one park record per blocker, and the
//!    stale-blocked release pass lifts it when they close.
//!
//! Every other operator-held item is a human's hold and is left alone. That
//! includes a PR held only by the retired `loom:operator-only`: since #11861
//! `loom:operator` parks a PR too, so the plain retired-label migration keeps
//! it parked and the #11795 Q1 "+ `loom:blocked`" pre-pass is not needed.
//!
//! **Order.** Run this pass *before* `labels migrate --apply --keep-labels`:
//! it reads the retired operator names too, so an escalation still labelled
//! `loom:operator-only` is moved straight to `loom:blocked` instead of
//! passing through `loom:operator`.
//!
//! Safety is `labels migrate`'s: dry run by default, the write-scope gate
//! before any call on `--apply`, open items only, add before remove, every
//! write paced under the shared secondary write limit (#11789), and a rate
//! limit stops the run (exit 4) for an idempotent rerun.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use loom_daemon::champion_hold::{standing_hold, HoldKind, HOLD_BY};
use loom_daemon::dep_classify::cli::EscalationMigration;
use loom_daemon::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
use serde::Serialize;

use super::labels_migrate::{classify, enc, pace_write, ForgeError, Target, PER_PAGE};

/// Pages read per label before a listing is treated as unanswerable.
const MAX_PAGES: u32 = 100;

/// The proposal and curated labels Pass 0 scanned for escalations.
pub(crate) const ESCALATION_LABELS: [&str; 4] = [
    "loom:curated",
    "loom:architect",
    "loom:hermit",
    "loom:auditor",
];

/// An open issue or PR and its labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HoldItem {
    pub number: u64,
    pub labels: Vec<String>,
    pub is_pr: bool,
}

/// The reads and writes this pass makes.
pub(crate) trait HoldsForge {
    /// Page `page` of OPEN issues and PRs carrying `label`.
    fn open_page(&self, label: &str, page: u32) -> Result<Vec<HoldItem>, ForgeError>;
    /// The bodies of an item's comments from trusted authors, oldest first.
    fn trusted_comments(&self, number: u64) -> Result<Vec<String>, ForgeError>;
    /// Write the hold's reason-only park record, then add `loom:blocked`.
    fn park_hold(&self, number: u64, kind: HoldKind) -> Result<(), ForgeError>;
    /// Remove `label` from one item ([`ForgeError::NotFound`]: not there).
    fn remove_label(&self, number: u64, label: &str) -> Result<(), ForgeError>;
    /// Hand one escalated proposal to the dependency classifier.
    fn migrate_escalation(&self, number: u64, apply: bool) -> EscalationMigration;
}

/// One report line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub(crate) enum Event {
    /// A Champion-held PR moved (or would move) to `loom:blocked`; `loom:pr`
    /// kept.
    Hold {
        item: u64,
        hold: &'static str,
        removed: Vec<String>,
    },
    /// An escalation whose blockers closed: the operator hold comes off.
    Release {
        item: u64,
    },
    /// An escalation still waiting: parked on these blockers.
    Park {
        item: u64,
        blocked_by: Vec<String>,
    },
    /// Left as it is.
    Left {
        item: u64,
        why: String,
    },
    Failed {
        item: u64,
        why: String,
    },
    Stopped {
        why: String,
    },
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Hold {
                item,
                hold,
                removed,
            } => write!(
                f,
                "HOLD #{item} {hold} -> loom:blocked + park record (loom:pr kept; removes {})",
                removed.join(", ")
            ),
            Event::Release { item } => {
                write!(f, "RELEASE #{item} (dependency-only, blockers closed)")
            }
            Event::Park { item, blocked_by } => {
                write!(f, "PARK #{item} -> loom:blocked, Blocked by {}", blocked_by.join(" "))
            }
            Event::Left { item, why } => write!(f, "LEFT #{item} ({why})"),
            Event::Failed { item, why } => write!(f, "FAILED #{item}: {why}"),
            Event::Stopped { why } => {
                write!(f, "STOPPED ({why}); rerun later, every step is idempotent")
            }
        }
    }
}

/// Counters for the `SUMMARY` line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(crate) struct Summary {
    pub holds: usize,
    pub released: usize,
    pub parked: usize,
    pub left: usize,
    /// Operator-held items neither shape matched: a human's holds.
    pub untouched: usize,
    pub writes: usize,
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
    pub rate_limited: bool,
}

impl Report {
    fn render(&self, out: &mut String) {
        let mode = if self.apply {
            "apply"
        } else {
            "dry-run (no forge writes; pass --apply to migrate)"
        };
        out.push_str(&format!("MODE holds {mode}\n"));
        for e in &self.events {
            out.push_str(&format!("{e}\n"));
        }
        let s = &self.summary;
        out.push_str(&format!(
            "SUMMARY holds={} released={} parked={} left={} untouched={} writes={} remaining={}\n",
            s.holds, s.released, s.parked, s.left, s.untouched, s.writes, s.remaining
        ));
    }
}

struct Stop;

fn stop_on_limit(r: &mut Report, e: &ForgeError) -> Result<(), Stop> {
    if let ForgeError::RateLimited(_) = e {
        r.events.push(Event::Stopped { why: e.to_string() });
        r.rate_limited = true;
        return Err(Stop);
    }
    Ok(())
}

/// Every open item carrying any operator-hold name, deduplicated, labels
/// merged, by number.
fn operator_held(forge: &dyn HoldsForge) -> Result<Vec<HoldItem>, ForgeError> {
    let mut all: BTreeMap<u64, HoldItem> = BTreeMap::new();
    for name in loom_daemon::work_finder::OPERATOR_HOLD_NAMES.iter() {
        let mut done = false;
        for page in 1..=MAX_PAGES {
            let batch = forge.open_page(name, page)?;
            let full = batch.len() >= PER_PAGE;
            for it in batch {
                all.entry(it.number).or_insert(it);
            }
            if !full {
                done = true;
                break;
            }
        }
        if !done {
            return Err(ForgeError::Other(format!(
                "more than {} open items carry {name}; listing truncated",
                MAX_PAGES as usize * PER_PAGE
            )));
        }
    }
    Ok(all.into_values().collect())
}

/// Run the pass on one repo.
pub(crate) fn run(forge: &dyn HoldsForge, repo: &str, apply: bool) -> Report {
    let mut r = Report {
        repo: repo.to_string(),
        apply,
        events: Vec::new(),
        summary: Summary::default(),
        exit: 0,
        rate_limited: false,
    };
    let items = match operator_held(forge) {
        Ok(i) => i,
        Err(e) => {
            if stop_on_limit(&mut r, &e).is_ok() {
                r.events.push(Event::Stopped {
                    why: format!("could not list operator-held items: {e}"),
                });
            }
            r.exit = 4;
            return r;
        }
    };
    for item in &items {
        if one(forge, item, apply, &mut r).is_err() {
            r.exit = 4;
            return r;
        }
    }
    if r.exit == 0 && r.summary.remaining > 0 {
        r.exit = 3;
    }
    r
}

fn one(forge: &dyn HoldsForge, item: &HoldItem, apply: bool, r: &mut Report) -> Result<(), Stop> {
    let has = |l: &str| item.labels.iter().any(|x| x == l);
    if loom_daemon::work_finder::is_frozen(&item.labels) {
        r.summary.untouched += 1;
        return Ok(());
    }
    if item.is_pr {
        return pr_hold(forge, item, apply, r);
    }
    if !ESCALATION_LABELS.iter().any(|l| has(l)) {
        r.summary.untouched += 1;
        return Ok(());
    }
    let n = item.number;
    match forge.migrate_escalation(n, apply) {
        EscalationMigration::Release => {
            r.events.push(Event::Release { item: n });
            r.summary.released += 1;
            count_write(r, apply);
        }
        EscalationMigration::Park(blocked_by) => {
            r.events.push(Event::Park {
                item: n,
                blocked_by,
            });
            r.summary.parked += 1;
            count_write(r, apply);
        }
        // Not an escalation at all (no record), or a merits one: a human's.
        EscalationMigration::Leave(why) if why == "no-escalation-record" => {
            r.summary.untouched += 1;
        }
        EscalationMigration::Leave(why) => {
            r.events.push(Event::Left { item: n, why });
            r.summary.left += 1;
        }
        EscalationMigration::Failed(why) => {
            if loom_daemon::rate_limit_breaker::indicates_rate_limit(&why) {
                return stop_on_limit(r, &ForgeError::RateLimited(why));
            }
            r.events.push(Event::Failed { item: n, why });
            r.summary.remaining += 1;
        }
    }
    Ok(())
}

/// A dry run leaves the work pending; an applied step counts as written.
fn count_write(r: &mut Report, apply: bool) {
    if apply {
        r.summary.writes += 1;
    } else {
        r.summary.remaining += 1;
    }
}

fn pr_hold(
    forge: &dyn HoldsForge,
    item: &HoldItem,
    apply: bool,
    r: &mut Report,
) -> Result<(), Stop> {
    let n = item.number;
    let comments = match forge.trusted_comments(n) {
        Ok(c) => c,
        Err(e) => {
            stop_on_limit(r, &e)?;
            r.events.push(Event::Failed {
                item: n,
                why: format!("comments unreadable: {e}"),
            });
            r.summary.remaining += 1;
            return Ok(());
        }
    };
    let Some(kind) = standing_hold(&comments) else {
        // A human's hold on a PR: `loom:operator` parks it as it is.
        r.summary.untouched += 1;
        return Ok(());
    };
    let hold = match kind {
        HoldKind::MergeRisk => "merge-risk",
        HoldKind::CriticalFile => "critical-file",
    };
    let removed: Vec<String> = item
        .labels
        .iter()
        .filter(|l| loom_daemon::work_finder::OPERATOR_HOLD_NAMES.contains(&l.as_str()))
        .cloned()
        .collect();
    r.events.push(Event::Hold {
        item: n,
        hold,
        removed: removed.clone(),
    });
    r.summary.holds += 1;
    if !apply {
        r.summary.remaining += 1;
        return Ok(());
    }
    // Add before remove: an interrupted run leaves the PR held twice, never
    // released, and `loom:pr` is never part of either write.
    if let Err(e) = forge.park_hold(n, kind) {
        stop_on_limit(r, &e)?;
        r.events.push(Event::Failed {
            item: n,
            why: format!("park failed, operator hold left in place: {e}"),
        });
        r.summary.remaining += 1;
        return Ok(());
    }
    r.summary.writes += 1;
    for l in &removed {
        match forge.remove_label(n, l) {
            Ok(()) => r.summary.writes += 1,
            Err(ForgeError::NotFound(_)) => {}
            Err(e) => {
                stop_on_limit(r, &e)?;
                r.events.push(Event::Failed {
                    item: n,
                    why: format!("remove {l} failed: {e}"),
                });
                r.summary.remaining += 1;
            }
        }
    }
    Ok(())
}

/// [`HoldsForge`] over `gh api` (REST), scoped to one repo and checkout.
pub(crate) struct GhHoldsForge {
    pub repo: String,
    pub root: PathBuf,
}

const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// Forge writes one escalation step may make (body, label add, label
/// removals, comment): paced up front so the step stays under the shared
/// write rate.
const ESCALATION_WRITES: usize = 4;

impl GhHoldsForge {
    fn gh(&self, intent: AccessIntent, args: &[&str]) -> Result<Vec<u8>, ForgeError> {
        let target = GhTarget::repo(&self.repo).map_err(ForgeError::Other)?;
        let what = format!("gh {}", args.get(..3).unwrap_or(args).join(" "));
        let r =
            GhInvocation::new(Operation::new("labels.migrate_holds"), intent, target, GH_TIMEOUT)
                .args(args)
                .run();
        if let Some(o) = r.ok_output() {
            return Ok(o.stdout.clone());
        }
        if let loom_daemon::cmd_out::CmdOutcome::Unavailable(
            loom_daemon::cmd_out::Unavailable::Shed { .. },
        ) = r
        {
            return Err(ForgeError::RateLimited(r.failure_reason(&what)));
        }
        Err(classify(r.failure_reason(&what)))
    }
}

/// One `[number, [label, ...], is_pr]` JSON line per item.
pub(crate) fn parse_hold_items(stdout: &str) -> Result<Vec<HoldItem>, ForgeError> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(|l| {
            serde_json::from_str::<(u64, Vec<String>, bool)>(l)
                .map(|(number, labels, is_pr)| HoldItem {
                    number,
                    labels,
                    is_pr,
                })
                .map_err(|e| ForgeError::Other(format!("bad item line {l:?}: {e}")))
        })
        .collect()
}

impl HoldsForge for GhHoldsForge {
    fn open_page(&self, label: &str, page: u32) -> Result<Vec<HoldItem>, ForgeError> {
        let path = format!(
            "repos/{}/issues?state=open&labels={}&per_page={PER_PAGE}&page={page}",
            self.repo,
            enc(label)
        );
        let jq = ".[] | [.number, [.labels[].name], (.pull_request != null)] | @json";
        let out = self.gh(AccessIntent::Read, &["api", &path, "--jq", jq])?;
        parse_hold_items(&String::from_utf8_lossy(&out))
    }

    fn trusted_comments(&self, number: u64) -> Result<Vec<String>, ForgeError> {
        let path = format!("repos/{}/issues/{number}/comments?per_page=100", self.repo);
        let out = self.gh(AccessIntent::Read, &["api", "--paginate", &path])?;
        loom_daemon::comment_trust::TrustPolicy::for_root(&self.root)
            .trusted_bodies(&out)
            .ok_or_else(|| ForgeError::Other("comment listing did not parse".to_string()))
    }

    fn park_hold(&self, number: u64, kind: HoldKind) -> Result<(), ForgeError> {
        // Two writes at most: the body record, then the label.
        pace_write();
        pace_write();
        let req = loom_daemon::park_record::apply::ApplyRequest {
            number,
            repo: Some(self.repo.clone()),
            blocked_by: Vec::new(),
            reason: Some(kind.reason().to_string()),
            by: Some(HOLD_BY.to_string()),
            at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            remove_labels: Vec::new(),
            dry_run: false,
        };
        let mut forge = loom_daemon::operator_decision::cli::GhForge::new(
            self.root.clone(),
            Some(self.repo.clone()),
        );
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = loom_daemon::park_record::apply::apply(&mut forge, &req, &mut out, &mut err);
        if code == loom_daemon::park_record::apply::exit::OK {
            return Ok(());
        }
        Err(classify(String::from_utf8_lossy(&err).trim().to_string()))
    }

    fn remove_label(&self, number: u64, label: &str) -> Result<(), ForgeError> {
        pace_write();
        let path = format!("repos/{}/issues/{number}/labels/{}", self.repo, enc(label));
        self.gh(AccessIntent::Write, &["api", "-X", "DELETE", &path, "--silent"])
            .map(|_| ())
    }

    fn migrate_escalation(&self, number: u64, apply: bool) -> EscalationMigration {
        if apply {
            for _ in 0..ESCALATION_WRITES {
                pace_write();
            }
        }
        let n = i64::try_from(number).unwrap_or(0);
        loom_daemon::dep_classify::cli::migrate_escalation(&self.root, &self.repo, n, apply)
    }
}

/// Run every target, gating `--apply` on write scope first; a rate limit ends
/// the fleet run (every repo shares the identity whose budget ran out).
pub(crate) fn run_targets(
    targets: &[Target],
    apply: bool,
    forge_for: &dyn Fn(&Target) -> Box<dyn HoldsForge>,
    gate: &dyn Fn(&Target) -> Result<(), String>,
) -> Vec<Report> {
    let mut reports = Vec::new();
    for t in targets {
        if apply {
            if let Err(why) = gate(t) {
                reports.push(Report {
                    repo: t.repo.clone(),
                    apply,
                    events: vec![Event::Stopped {
                        why: format!("refusing --apply (#9548): {why}"),
                    }],
                    summary: Summary::default(),
                    exit: 2,
                    rate_limited: false,
                });
                continue;
            }
        }
        let r = run(forge_for(t).as_ref(), &t.repo, apply);
        let stop = r.rate_limited;
        reports.push(r);
        if stop {
            break;
        }
    }
    reports
}

/// Render the reports; returns the exit code.
pub(crate) fn finish(reports: &[Report], apply: bool, fleet: bool, json: bool) -> i32 {
    let exit = reports.iter().map(|r| r.exit).max().unwrap_or(0);
    if json {
        let v =
            serde_json::json!({ "apply": apply, "holds": true, "repos": reports, "exit": exit });
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return exit;
    }
    let mut out = String::new();
    for r in reports {
        if fleet {
            out.push_str(&format!("== {} ==\n", r.repo));
        }
        r.render(&mut out);
    }
    print!("{out}");
    if exit == 3 && !apply {
        eprintln!("labels migrate --holds: dry run only; rerun with --apply to migrate");
    }
    exit
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "labels_migrate_holds_tests.rs"]
mod tests;
