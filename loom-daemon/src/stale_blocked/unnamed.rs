//! The `loom:blocked-unnamed` queue pass (#10558): hand every **undocumented**
//! `loom:blocked` issue to Curator, which is scheduled fleet-wide.
//!
//! # The gap this closes
//!
//! [`super::classify`] reports a `loom:blocked` issue naming no blocker and
//! stating no reason as [`Verdict::Undocumented`]. Nothing acted on it: Guide's
//! unblock sweep skips it, and Curator's discovery queries all exclude
//! `loom:blocked`. Those blocks never re-entered any queue.
//!
//! # What the pass does (one tick, beside the #10556 release pass)
//!
//! - **Queue**: label an undocumented `loom:blocked` **issue** (PRs are
//!   Judge/Doctor's lane) with [`UNNAMED_LABEL`], one write, only if absent.
//!   Curator drains the label (`unnamed-block-review.md`): it names the
//!   blocker, parks with a stated reason (which makes the issue
//!   [`Verdict::HeldWithReason`], so it is never re-queued), or releases it.
//! - **Clear**: remove [`UNNAMED_LABEL`] from any issue that is no longer
//!   `loom:blocked` or no longer `Undocumented`.
//!
//! # Skips — counted, never written
//!
//! See [`Skip`]: legacy daemon-hold comments (pre-#10161), operator-ruled
//! permanent blocks (#8742), human-held labels, an active claim, a concurrent
//! edit, the per-pass cap. An archived repository, a refused breaker, and any
//! unevaluated read (a failed read is never treated as "undocumented") write
//! nothing at all.
//!
//! The write path is the caller's: [`super::release_gh::maybe_run`] runs this
//! only inside the same forge-write scope gate (#9548) as the release pass.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Value;

use super::batch::{gather_filtered, Options, StaleBlockedForge};
use super::budget::Floor;
use super::release::{
    has_operator_hold, same_body, Config, ReleaseForge, Unread, PERMANENT_BLOCK_MARKER,
};
use super::{classify, Artifact, Verdict};
use crate::comment_trust::TrustPolicy;
use crate::forge_identity::FleetLogins;
use crate::park_record::apply::{ParkForge, BLOCKED_LABEL};
use crate::park_record::blockers;
use crate::sweep_registry::{PRLESS_HOLD_COMMENT_MARKER, QUARANTINE_COMMENT_MARKER};

/// The queue label: applied here, removed by Curator or by [`run`].
pub const UNNAMED_LABEL: &str = "loom:blocked-unnamed";

/// An active claim: someone is already working the issue.
const CLAIM_LABELS: [&str; 2] = ["loom:curating", "loom:building"];

/// Why an undocumented issue was not queued. Every variant is counted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Skip {
    /// A trusted PR-less-retry or quarantine hold comment (written before
    /// #10161's body record).
    DaemonHold,
    /// `<!-- loom:permanent-block` in the body or any comment (#8742).
    Permanent,
    /// A human-hold label is on the issue (#10001's lane).
    OperatorHold,
    /// `loom:curating` or `loom:building`.
    ActiveClaim,
    /// The issue changed between the listing and the write.
    ConcurrentEdit,
    /// Over this pass's write cap; next pass.
    WriteCap,
}

impl Skip {
    /// The kebab-case key used in the log line.
    #[must_use]
    pub fn key(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }
}

/// One pass's outcome: the counts the dashboard charts.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub dry_run: bool,
    pub archived: bool,
    /// The probe, the breaker or the listing refused the pass.
    pub enumerate_error: Option<String>,
    /// `unnamed_queued`: issues labelled this pass.
    pub queued: Vec<u64>,
    /// `unnamed_already_queued`: undocumented issues that already carry it.
    pub already_queued: usize,
    /// `unnamed_cleared`: issues the label was removed from.
    pub cleared: Vec<u64>,
    /// `unnamed_skipped{reason}`.
    pub skipped: BTreeMap<String, usize>,
    pub unevaluated: Vec<Unread>,
    pub failed: Vec<Unread>,
}

impl Report {
    fn skip(&mut self, why: Skip) {
        *self.skipped.entry(why.key()).or_default() += 1;
    }

    fn unread(&mut self, number: u64, why: impl Into<String>) {
        self.unevaluated.push(Unread {
            number,
            why: why.into(),
        });
    }

    /// One log line with every count.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut skipped = String::new();
        for (k, v) in &self.skipped {
            let _ = write!(skipped, "{}{k}={v}", if skipped.is_empty() { "" } else { "," });
        }
        let mut line = format!(
            "unnamed_queued={} unnamed_already_queued={} unnamed_cleared={} \
             unnamed_skipped{{{skipped}}} unevaluated={} failed={}",
            self.queued.len(),
            self.already_queued,
            self.cleared.len(),
            self.unevaluated.len(),
            self.failed.len(),
        );
        if self.dry_run {
            line.push_str(" (dry-run: nothing written)");
        }
        if self.archived {
            line.push_str(" (archived repository: not evaluated)");
        }
        if let Some(e) = &self.enumerate_error {
            let _ = write!(line, " (not evaluated: {e})");
        }
        line
    }
}

fn has_label(labels: &[String], wanted: &str) -> bool {
    labels.iter().any(|l| l == wanted)
}

/// The vetoes visible in the labels and body alone.
fn label_skip(body: &str, labels: &[String]) -> Option<Skip> {
    if has_operator_hold(labels) {
        Some(Skip::OperatorHold)
    } else if labels.iter().any(|l| CLAIM_LABELS.contains(&l.as_str())) {
        Some(Skip::ActiveClaim)
    } else if body.contains(PERMANENT_BLOCK_MARKER) {
        Some(Skip::Permanent)
    } else {
        None
    }
}

/// Run one pass.
pub fn run(
    gather: &mut dyn StaleBlockedForge,
    park: &mut dyn ParkForge,
    extra: &mut dyn ReleaseForge,
    fleet: &FleetLogins,
    policy: &TrustPolicy,
    cfg: &Config,
) -> Report {
    let mut report = Report {
        dry_run: cfg.dry_run,
        ..Report::default()
    };
    match extra.archived() {
        Ok(false) => {}
        Ok(true) => {
            report.archived = true;
            return report;
        }
        Err(e) => {
            report.enumerate_error = Some(format!("archived-repository probe failed: {e}"));
            return report;
        }
    }
    if gather.breaker_open() {
        report.enumerate_error =
            Some("rate-limit breaker is suppressing forge calls — not evaluated this pass".into());
        return report;
    }
    let rows = match gather.list_blocked() {
        Ok(rows) => rows,
        Err(e) => {
            report.enumerate_error = Some(format!("loom:blocked listing failed: {e}"));
            return report;
        }
    };
    let labeled = match gather.list_unnamed() {
        Ok(l) => l,
        Err(e) => {
            // Without the queue listing nothing can be cleared or counted as
            // queued; evaluating further would only guess.
            report.enumerate_error = Some(format!("{UNNAMED_LABEL} listing failed: {e}"));
            return report;
        }
    };

    let blocked_numbers: HashSet<u32> = rows.iter().map(|r| r.number).collect();
    let mut cap = Cap::new(cfg.max_writes);

    // Anything labelled that is no longer a blocked issue loses the label.
    for r in labeled
        .iter()
        .filter(|r| !blocked_numbers.contains(&r.number))
    {
        clear(park, u64::from(r.number), cfg.dry_run, &mut cap, &mut report);
    }

    // A numbered park record is a documented block, so the evidence read is
    // only for issues without one.
    let issues: Vec<_> = rows.into_iter().filter(|r| !r.is_pull_request).collect();
    let mut wanted: HashSet<i64> = HashSet::new();
    let mut candidates = Vec::new();
    for row in issues {
        let body = row.body.clone().unwrap_or_default();
        let queued = has_label(&row.labels, UNNAMED_LABEL);
        if !blockers(&body).is_empty() {
            if queued {
                clear(park, u64::from(row.number), cfg.dry_run, &mut cap, &mut report);
            }
            continue;
        }
        if !queued {
            if let Some(why) = label_skip(&body, &row.labels) {
                report.skip(why);
                continue;
            }
        }
        wanted.insert(i64::from(row.number));
        candidates.push((queued, row));
    }
    if candidates.is_empty() {
        return report;
    }

    let mut bodies: HashMap<i64, String> = HashMap::new();
    let gathering = gather_filtered(
        gather,
        fleet,
        Options {
            limit: u32::MAX,
            no_prs: true,
            floor: Floor::default(),
        },
        &mut |_, n, input| {
            let keep = wanted.contains(&n);
            if keep {
                bodies.insert(n, input.body.clone());
            }
            keep
        },
    );
    let evidence: HashMap<u64, Result<super::Evidence, String>> = gathering
        .items
        .into_iter()
        .filter(|g| g.kind == Artifact::Issue)
        .filter_map(|g| u64::try_from(g.number).ok().map(|n| (n, g.evidence)))
        .collect();

    for (queued, row) in candidates {
        let n = u64::from(row.number);
        let verdict = match evidence.get(&n) {
            Some(Ok(ev)) => classify(ev),
            Some(Err(why)) => {
                report.unread(n, why.clone());
                continue;
            }
            None => {
                report.unread(n, "no evidence gathered");
                continue;
            }
        };
        let undocumented = verdict == Verdict::Undocumented;
        match (queued, undocumented) {
            (true, true) => report.already_queued += 1,
            (true, false) => clear(park, n, cfg.dry_run, &mut cap, &mut report),
            (false, false) => {}
            (false, true) => {
                let planned = row.body.as_deref().unwrap_or_default();
                let evidence_body = bodies.get(&i64::from(row.number));
                if !evidence_body.is_some_and(|b| same_body(b, planned)) {
                    report.skip(Skip::ConcurrentEdit);
                    continue;
                }
                queue(park, extra, policy, n, planned, cfg.dry_run, &mut cap, &mut report);
            }
        }
    }
    report
}

/// The per-pass write budget: queueing and clearing each get `max`.
struct Cap {
    max: usize,
    queued: usize,
    cleared: usize,
}

impl Cap {
    fn new(max: usize) -> Self {
        Self {
            max,
            queued: 0,
            cleared: 0,
        }
    }
}

fn clear(park: &mut dyn ParkForge, n: u64, dry_run: bool, cap: &mut Cap, report: &mut Report) {
    if cap.cleared >= cap.max {
        report.skip(Skip::WriteCap);
        return;
    }
    cap.cleared += 1;
    if !dry_run {
        if let Err(e) = park.remove_label(n, UNNAMED_LABEL) {
            report.failed.push(Unread {
                number: n,
                why: format!("removing {UNNAMED_LABEL} failed: {e}"),
            });
            return;
        }
    }
    report.cleared.push(n);
}

/// Re-read, check the comment-borne vetoes, then label.
#[allow(clippy::too_many_arguments)]
fn queue(
    park: &mut dyn ParkForge,
    extra: &mut dyn ReleaseForge,
    policy: &TrustPolicy,
    n: u64,
    planned_body: &str,
    dry_run: bool,
    cap: &mut Cap,
    report: &mut Report,
) {
    if cap.queued >= cap.max {
        report.skip(Skip::WriteCap);
        return;
    }
    let fresh = match park.view(n) {
        Ok(s) => s,
        Err(e) => {
            report.unread(n, format!("fresh read before the write failed: {e}"));
            return;
        }
    };
    if !has_label(&fresh.labels, BLOCKED_LABEL)
        || has_label(&fresh.labels, UNNAMED_LABEL)
        || !same_body(&fresh.body, planned_body)
    {
        report.skip(Skip::ConcurrentEdit);
        return;
    }
    if let Some(why) = label_skip(&fresh.body, &fresh.labels) {
        report.skip(why);
        return;
    }
    let comments = match extra.comments(n) {
        Ok(c) => c,
        Err(e) => {
            report.unread(n, format!("comment read failed: {e}"));
            return;
        }
    };
    let body_of = |c: &Value| {
        c.get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    if comments
        .iter()
        .any(|c| body_of(c).contains(PERMANENT_BLOCK_MARKER))
    {
        report.skip(Skip::Permanent);
        return;
    }
    if comments.iter().filter(|c| policy.trusts_json(c)).any(|c| {
        let b = body_of(c);
        b.contains(PRLESS_HOLD_COMMENT_MARKER) || b.contains(QUARANTINE_COMMENT_MARKER)
    }) {
        report.skip(Skip::DaemonHold);
        return;
    }
    cap.queued += 1;
    if !dry_run {
        if let Err(e) = park.add_labels(n, &[UNNAMED_LABEL.to_string()]) {
            report.failed.push(Unread {
                number: n,
                why: format!("adding {UNNAMED_LABEL} failed: {e}"),
            });
            return;
        }
    }
    report.queued.push(n);
}
