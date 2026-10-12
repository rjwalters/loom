//! `release-stale-blocked` — the deterministic, no-LLM `loom:blocked` release
//! pass (#10556).
//!
//! # The gap this closes
//!
//! Nothing in the daemon removed `loom:blocked`. `check-stale-blocked` is
//! read-only by contract, `notify-cleared-blockers` only comments, and the one
//! remover — Guide's `check_and_unblock` shell snippet — does not run where
//! Guide is not in the role-runner rotation. Parks whose recorded blockers had
//! all closed stayed parked.
//!
//! # The decision rule: park-record blockers only
//!
//! Only blockers **declared in a body park record**
//! (`<!-- loom:park Blocked by: #N -->`, [`crate::park_record`]) are acted on.
//! The full [`super::classify`] verdict also fires on prose, checklist and
//! closing-PR evidence, which are #9274's false-positive classes; here they
//! can only *veto* a release, never cause one.
//!
//! | Declared blockers | Action |
//! |---|---|
//! | every one an issue `CLOSED` or a PR `MERGED` | **release**: drop `loom:blocked`, restore the prior lane label |
//! | some resolved, some open | **re-park**: rewrite the body keeping only the open records; keep the label |
//! | none resolved | nothing (still blocked) |
//!
//! # Skips — counted, never written
//!
//! See [`Skip`]. Each is the conservative direction: a skip costs one more
//! tick of a stale label, a wrong release walks over a real hold.
//!
//! # Cost
//!
//! Phase 1 reads only the ETag'd `loom:blocked` listing and one ETag'd REST
//! state per distinct declared blocker, so a steady-state pass costs no
//! GraphQL at all. Phase 2 runs [`super::batch::gather_filtered`] (budget
//! floor, fail-safe answers, #10480) keeping **release candidates only**.
//!
//! # Writes
//!
//! Every write re-reads the artifact first and aborts if `loom:blocked` or
//! its park records changed since the plan. The audit comment (carrying an
//! idempotency marker) is posted before any label/body edit; a re-run that
//! finds a trusted marker for the same blocker set skips the comment and only
//! retries the edit. The lane label is added *before* `loom:blocked` is
//! removed, so a failed removal is retried next pass rather than losing the
//! restore.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Value;

use super::batch::{gather_filtered, Options, RefState, StaleBlockedForge};
use super::budget::{Floor, ForgeCost, Guard};
use super::release_items::{Item, ItemContext, ItemVerdict};
use super::{classify, Artifact, Verdict};
use crate::comment_trust::TrustPolicy;
use crate::forge_identity::FleetLogins;
use crate::forge_listing::RestIssue;
use crate::park_record::apply::{ParkForge, BLOCKED_LABEL};
use crate::park_record::{blockers, drop_blockers, has_qualified_ref, parse, BlockerRef};
use crate::sweep_registry::{PRLESS_HOLD_COMMENT_MARKER, QUARANTINE_COMMENT_MARKER};

/// The release audit comment's idempotency marker prefix:
/// `<!-- loom:stale-blocked-release:#1,#2 -->`.
pub const RELEASE_MARKER_PREFIX: &str = "<!-- loom:stale-blocked-release:";

/// The re-park audit comment's idempotency marker prefix.
pub const REPARK_MARKER_PREFIX: &str = "<!-- loom:stale-blocked-repark:";

/// An operator-ruled permanent block (#8742). Honoured from **any** author:
/// it can only prevent a write.
pub const PERMANENT_BLOCK_MARKER: &str = "<!-- loom:permanent-block";

/// Labels that mean a human decision is pending on the artifact itself.
/// `loom:operator-priority` (the star) is deliberately absent: it is not a
/// hold.
pub const OPERATOR_HOLD_LABELS: [&str; 5] = [
    "loom:operator",
    "loom:operator-only",
    "loom:operator-blocked",
    "loom:operator-mechanical",
    "loom:operator-decision",
];

const ISSUE_LABEL: &str = "loom:issue";
const REVIEW_REQUESTED: &str = "loom:review-requested";
const CHANGES_REQUESTED: &str = "loom:changes-requested";

/// The reads and writes the pass needs beyond [`StaleBlockedForge`] (evidence)
/// and [`ParkForge`] (fresh view, body and label writes).
pub trait ReleaseForge {
    /// Whether the repository is archived (read once per pass).
    ///
    /// # Errors
    /// The read failed.
    fn archived(&mut self) -> Result<bool, String>;

    /// Every comment on an issue or PR as REST objects (body, user,
    /// `author_association`), oldest first.
    ///
    /// # Errors
    /// The read failed.
    fn comments(&mut self, number: u64) -> Result<Vec<Value>, String>;

    /// The label names of every `labeled` event, oldest first.
    ///
    /// # Errors
    /// The read failed.
    fn labeled_events(&mut self, number: u64) -> Result<Vec<String>, String>;

    /// Post one comment.
    ///
    /// # Errors
    /// The write failed or was refused.
    fn post_comment(&mut self, number: u64, is_pr: bool, body: &str) -> Result<(), String>;
}

/// Why an artifact was left alone. Every variant is counted, none writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Skip {
    /// No park record in the body: a prose-only park (#10558's job).
    NoParkRecord,
    /// A `(unstated)` record: a reason-only or deliberate hold (#10558).
    Unstated,
    /// A qualified `OWNER/REPO#N` blocker (#10443). This pass resolves local
    /// blockers only, so a park naming another repo's artifact is left alone
    /// whole — even when every local blocker beside it has resolved.
    CrossRepo,
    /// A declared blocker PR closed **without** merging (#9274 class 1).
    ClosedUnmergedPr,
    /// Another cited reference (prose or an unchecked `## Dependencies`
    /// entry) is still open.
    OtherOpenReference,
    /// An issue with an open closing PR.
    OpenClosingPr,
    /// An unticked `## Dependencies` box (#9274): its ref may have resolved,
    /// but an unchecked box is unmet until a human confirms its whole
    /// condition, so the pass never releases over it.
    UntickedChecklist,
    /// A PR whose own state supersedes the cleared park
    /// ([`super::park_self_block`]).
    Superseded,
    /// An operator label is on the artifact.
    OperatorHold,
    /// `<!-- loom:permanent-block` in the body or any comment (#8742).
    Permanent,
    /// A fleet PR-less-retry hold or quarantine comment (#10161).
    DaemonHold,
    /// The artifact changed between the plan and the write: its body differs
    /// from the one the plan and its evidence were read from, its park record
    /// changed, or it gained a body/label veto.
    ConcurrentEdit,
    /// Over this pass's write cap; next pass.
    WriteCap,
}

impl Skip {
    /// The kebab-case key used in the log line and `--json`.
    #[must_use]
    pub fn key(self) -> String {
        serde_json::to_value(self)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default()
    }
}

/// How the pass is configured.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// Plan and report; write nothing.
    pub dry_run: bool,
    /// Artifacts acted on (released or re-parked) per pass.
    pub max_writes: usize,
    /// The remaining-points floor (#10480).
    pub floor: Floor,
}

/// One artifact released or re-parked (or planned, under `dry_run`).
#[derive(Debug, Clone, Serialize)]
pub struct Acted {
    pub kind: &'static str,
    pub number: u64,
    /// The declared blockers found resolved.
    pub resolved: Vec<u64>,
    /// The declared blockers still open (re-park only).
    pub still_open: Vec<u64>,
    /// The lane label restored on release, if any.
    pub restored: Option<String>,
    /// The audit comment was posted this pass (false when a trusted marker
    /// already carried it, or under `dry_run`).
    pub commented: bool,
    /// Every write landed (false under `dry_run`).
    pub applied: bool,
}

/// An artifact that could not be evaluated, or whose write failed.
#[derive(Debug, Clone, Serialize)]
pub struct Unread {
    pub number: u64,
    pub why: String,
}

/// One pass's outcome: the per-outcome counts the dashboard charts.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Report {
    pub dry_run: bool,
    pub archived: bool,
    /// The listing (or the archived probe, or the breaker) refused the pass.
    pub enumerate_error: Option<String>,
    pub examined: usize,
    pub released: Vec<Acted>,
    pub reparked: Vec<Acted>,
    pub still_blocked: usize,
    pub skipped: BTreeMap<String, usize>,
    pub unevaluated: Vec<Unread>,
    pub failed: Vec<Unread>,
    pub cost: ForgeCost,
    /// One verdict per listed artifact, in decision order (#10752).
    pub items: Vec<Item>,
    #[serde(skip)]
    pub(super) context: ItemContext,
}

impl Report {
    fn skip(&mut self, number: u64, why: Skip) {
        *self.skipped.entry(why.key()).or_default() += 1;
        self.decide(number, ItemVerdict::Skipped, Some(why.key()), None);
    }

    fn unread(&mut self, number: u64, why: impl Into<String>) {
        let why = why.into();
        self.decide(number, ItemVerdict::Unevaluated, None, Some(&why));
        self.unevaluated.push(Unread { number, why });
    }

    /// One log line with every count.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut skipped = String::new();
        for (k, v) in &self.skipped {
            let _ = write!(skipped, "{}{k}={v}", if skipped.is_empty() { "" } else { "," });
        }
        let mut line = format!(
            "released={} reparked={} still_blocked={} skipped{{{skipped}}} unevaluated={} \
             failed={} examined={}",
            self.released.len(),
            self.reparked.len(),
            self.still_blocked,
            self.unevaluated.len(),
            self.failed.len(),
            self.examined,
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

/// What phase 1 decided for one artifact.
#[derive(Debug, Clone)]
struct Plan {
    kind: Artifact,
    row: RestIssue,
    declared: Vec<u64>,
    resolved: Vec<u64>,
    open: Vec<u64>,
}

impl Plan {
    fn releases(&self) -> bool {
        self.open.is_empty()
    }
}

/// Whether a declared blocker's state means "resolved" — an issue closed, or
/// a PR merged. `None` for a PR closed without merging, which is never
/// resolved and vetoes the artifact.
fn resolution(s: &RefState) -> Option<bool> {
    match (s.state.as_str(), s.is_pr) {
        ("MERGED", _) | ("CLOSED", false) => Some(true),
        ("CLOSED", true) => None,
        _ => Some(false),
    }
}

fn has_operator_hold(labels: &[String]) -> bool {
    labels
        .iter()
        .any(|l| OPERATOR_HOLD_LABELS.contains(&l.as_str()))
}

/// The pre-read vetoes visible in the body and labels alone.
fn body_skip(body: &str, labels: &[String]) -> Option<Skip> {
    let records = parse(body);
    if records.iter().any(|r| r.blocker.is_none()) {
        return Some(Skip::Unstated);
    }
    if records.is_empty() {
        return Some(Skip::NoParkRecord);
    }
    if has_qualified_ref(body) {
        return Some(Skip::CrossRepo);
    }
    if has_operator_hold(labels) {
        return Some(Skip::OperatorHold);
    }
    if body.contains(PERMANENT_BLOCK_MARKER) {
        return Some(Skip::Permanent);
    }
    None
}

/// Whether two reads of one body are the same text, up to line endings and
/// trailing whitespace.
///
/// Any other edit counts: a prose `Depends on #9` or an unchecked
/// `## Dependencies` box the evidence never saw must abort the write rather
/// than be released over.
fn same_body(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.replace("\r\n", "\n")
            .lines()
            .map(str::trim_end)
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
            .to_string()
    };
    norm(a) == norm(b)
}

/// The listing body a plan was built from.
fn planned_body(plan: &Plan) -> &str {
    plan.row.body.as_deref().unwrap_or_default()
}

/// The park's declared blockers as local numbers, or `None` when any is
/// qualified (`OWNER/REPO#N`, #10443).
///
/// The `None` is the point: dropping a qualified ref here would let a
/// local-only check release an issue whose cross-repo blocker is still open.
fn local_blockers(body: &str) -> Option<Vec<u64>> {
    blockers(body)
        .iter()
        .map(|b: &BlockerRef| b.repo.is_none().then_some(b.number))
        .collect()
}

/// The idempotency marker for one action over `resolved`.
#[must_use]
pub fn marker(release: bool, resolved: &[u64]) -> String {
    let list = resolved
        .iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(",");
    let prefix = if release {
        RELEASE_MARKER_PREFIX
    } else {
        REPARK_MARKER_PREFIX
    };
    format!("{prefix}{list} -->")
}

fn refs(ns: &[u64]) -> String {
    ns.iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The audit comment body.
fn comment_body(plan: &Plan, restored: Option<&str>) -> String {
    let mark = marker(plan.releases(), &plan.resolved);
    if plan.releases() {
        let restore = match (restored, plan.kind) {
            (Some(l), _) => format!(" Restored `{l}`."),
            (None, Artifact::Issue) => format!(
                " `{ISSUE_LABEL}` was not restored: this issue never carried it before the \
                 block, so it re-enters triage/approval rather than the Builder queue."
            ),
            (None, Artifact::Pr) => String::new(),
        };
        format!(
            "**Unblocked** by the daemon's deterministic release pass (#10556): every blocker \
             declared in this {}'s park record has resolved ({}). Removed `{BLOCKED_LABEL}`.\
             {restore}\n\n{mark}",
            plan.kind.label(),
            refs(&plan.resolved),
        )
    } else {
        format!(
            "**Re-parked** by the daemon's deterministic release pass (#10556): declared \
             blocker(s) {} resolved, so their park records were removed from the body. Still \
             blocked by {}; `{BLOCKED_LABEL}` stays.\n\n{mark}",
            refs(&plan.resolved),
            refs(&plan.open),
        )
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
        cost: ForgeCost {
            floor: cfg.floor,
            ..ForgeCost::default()
        },
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
    report.examined = rows.len();
    let plans = plan_from_listing(gather, rows, cfg.floor, &mut report);
    let plans = veto_from_evidence(gather, fleet, plans, cfg.floor, &mut report);
    let mut writes = 0;
    for plan in plans {
        if writes >= cfg.max_writes {
            report.skip(u64::from(plan.row.number), Skip::WriteCap);
            continue;
        }
        if execute(park, extra, policy, &plan, cfg.dry_run, &mut report) {
            writes += 1;
        }
    }
    report
}

/// Phase 1: the listing body alone, plus one ETag'd state read per distinct
/// declared blocker.
fn plan_from_listing(
    gather: &mut dyn StaleBlockedForge,
    rows: Vec<RestIssue>,
    floor: Floor,
    report: &mut Report,
) -> Vec<Plan> {
    let mut eligible = Vec::new();
    for row in rows {
        report.listed(&row);
        let number = u64::from(row.number);
        let body = row.body.clone().unwrap_or_default();
        match body_skip(&body, &row.labels) {
            Some(why) => report.skip(number, why),
            None => match local_blockers(&body) {
                Some(declared) => eligible.push((declared, row)),
                None => report.skip(number, Skip::CrossRepo),
            },
        }
    }
    if eligible.is_empty() {
        return Vec::new();
    }
    if let Some(b) = gather.budget() {
        if b.core_remaining < floor.core {
            let why = format!(
                "budget floor: core remaining {} < floor {} — not evaluated this pass",
                b.core_remaining, floor.core
            );
            for (_, row) in &eligible {
                report.unread(u64::from(row.number), why.clone());
            }
            report.cost.budget_refused = Some(why);
            return Vec::new();
        }
    }
    let mut guard = Guard::new(floor);
    let mut states: HashMap<u64, Result<RefState, String>> = HashMap::new();
    let mut plans = Vec::new();
    'rows: for (declared, row) in eligible {
        let number = u64::from(row.number);
        let (mut resolved, mut open) = (Vec::new(), Vec::new());
        for &b in &declared {
            let state = states
                .entry(b)
                .or_insert_with(|| {
                    guard.core(&gather.meter())?;
                    let n = i64::try_from(b).map_err(|e| e.to_string())?;
                    match gather.ref_state(None, n) {
                        Ok(Some(s)) if !s.state.is_empty() => Ok(s),
                        Ok(Some(_)) => Err("the forge returned no state".to_string()),
                        Ok(None) => Err("not found (HTTP 404)".to_string()),
                        Err(e) => Err(e.to_string()),
                    }
                })
                .clone();
            report.blocker_read(b, &state);
            match state {
                Err(why) => {
                    report.unread(number, format!("declared blocker #{b}: {why}"));
                    continue 'rows;
                }
                Ok(s) => match resolution(&s) {
                    None => {
                        report.skip(number, Skip::ClosedUnmergedPr);
                        continue 'rows;
                    }
                    Some(true) => resolved.push(b),
                    Some(false) => open.push(b),
                },
            }
        }
        if resolved.is_empty() {
            report.hold(number);
            continue;
        }
        let kind = if row.is_pull_request {
            Artifact::Pr
        } else {
            Artifact::Issue
        };
        plans.push(Plan {
            kind,
            row,
            declared,
            resolved,
            open,
        });
    }
    report.cost.meter = gather.meter();
    report.cost.budget_stopped = guard.stopped();
    plans
}

/// Phase 2: the full evidence, for release candidates only, may veto a
/// release. A re-park only narrows the record, so it needs no evidence.
fn veto_from_evidence(
    gather: &mut dyn StaleBlockedForge,
    fleet: &FleetLogins,
    plans: Vec<Plan>,
    floor: Floor,
    report: &mut Report,
) -> Vec<Plan> {
    let wanted: HashSet<i64> = plans
        .iter()
        .filter(|p| p.releases())
        .map(|p| i64::from(p.row.number))
        .collect();
    if wanted.is_empty() {
        return plans;
    }
    let opts = Options {
        limit: u32::MAX,
        no_prs: false,
        floor,
    };
    // The listing is ETag'd (a free 304 after phase 1's read); only the
    // candidates go on to the closing-PR GraphQL and state reads.
    // Keep the body each candidate's evidence was read from: a release is
    // only sound over the text that evidence describes.
    let mut bodies: HashMap<i64, String> = HashMap::new();
    let gathering = gather_filtered(gather, fleet, opts, &mut |_, n, input| {
        let keep = wanted.contains(&n);
        if keep {
            bodies.insert(n, input.body.clone());
        }
        keep
    });
    report.cost.meter = gathering.cost.meter;
    report.cost.budget_before = gathering.cost.budget_before;
    report.cost.projected = gathering.cost.projected;
    if gathering.cost.budget_refused.is_some() {
        report.cost.budget_refused = gathering.cost.budget_refused.clone();
    }
    if gathering.cost.budget_stopped.is_some() {
        report.cost.budget_stopped = gathering.cost.budget_stopped.clone();
    }
    let evidence: HashMap<u64, Result<super::Evidence, String>> = gathering
        .items
        .into_iter()
        .filter_map(|g| u64::try_from(g.number).ok().map(|n| (n, g.evidence)))
        .collect();
    let mut kept = Vec::new();
    for plan in plans {
        if !plan.releases() {
            kept.push(plan);
            continue;
        }
        let n = u64::from(plan.row.number);
        let ev = match evidence.get(&n) {
            Some(Ok(ev)) => ev,
            Some(Err(why)) => {
                report.unread(n, why.clone());
                continue;
            }
            None => {
                report.unread(n, "no evidence gathered");
                continue;
            }
        };
        match bodies.get(&i64::from(plan.row.number)) {
            Some(b) if same_body(b, planned_body(&plan)) => {}
            Some(_) => {
                report.skip(n, Skip::ConcurrentEdit);
                continue;
            }
            None => {
                report.unread(n, "the body the evidence was read from is unknown");
                continue;
            }
        }
        let other_open = ev.prose.iter().any(|r| r.state == "OPEN")
            || ev
                .named
                .iter()
                .any(|d| !d.checked && d.state.as_deref() == Some("OPEN"));
        let veto = if other_open {
            Some(Skip::OtherOpenReference)
        } else if plan.kind == Artifact::Issue && ev.closing.iter().any(|p| p.state == "OPEN") {
            Some(Skip::OpenClosingPr)
        } else if ev.unparsed_unchecked > 0 || ev.named.iter().any(|d| !d.checked) {
            // Checked here, not only via `classify`: a resolved park blocker
            // also arrives as prose, and the advisory's prose rule then reads
            // `Stale` over an unticked checklist (#9274).
            Some(Skip::UntickedChecklist)
        } else {
            match classify(ev) {
                Verdict::Stale(_) => None,
                Verdict::Superseded { .. } => Some(Skip::Superseded),
                Verdict::Unticked { .. } => Some(Skip::UntickedChecklist),
                // Unreachable with every declared blocker resolved; never
                // release on a verdict that does not say so.
                Verdict::StillBlocked | Verdict::Undocumented => Some(Skip::OtherOpenReference),
            }
        };
        match veto {
            Some(why) => report.skip(n, why),
            None => kept.push(plan),
        }
    }
    kept
}

/// Phase 3: re-read, veto, comment, edit. Returns whether this artifact
/// counts against the write cap (planned or written).
fn execute(
    park: &mut dyn ParkForge,
    extra: &mut dyn ReleaseForge,
    policy: &TrustPolicy,
    plan: &Plan,
    dry_run: bool,
    report: &mut Report,
) -> bool {
    let n = u64::from(plan.row.number);
    let fresh = match park.view(n) {
        Ok(s) => s,
        Err(e) => {
            report.unread(n, format!("fresh read before the write failed: {e}"));
            return false;
        }
    };
    // The fresh body must be the one the plan (and, for a release, the
    // evidence — checked equal in phase 2) was read from. Comparing only the
    // park record would release over a `Depends on #9` added since.
    let unchanged = fresh.labels.iter().any(|l| l == BLOCKED_LABEL)
        && same_body(&fresh.body, planned_body(plan))
        && local_blockers(&fresh.body).as_ref() == Some(&plan.declared)
        && body_skip(&fresh.body, &fresh.labels).is_none();
    if !unchanged {
        report.skip(n, Skip::ConcurrentEdit);
        return false;
    }
    let comments = match extra.comments(n) {
        Ok(c) => c,
        Err(e) => {
            report.unread(n, format!("comment read failed: {e}"));
            return false;
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
        report.skip(n, Skip::Permanent);
        return false;
    }
    let trusted: Vec<String> = comments
        .iter()
        .filter(|c| policy.trusts_json(c))
        .map(body_of)
        .collect();
    if trusted
        .iter()
        .any(|b| b.contains(PRLESS_HOLD_COMMENT_MARKER) || b.contains(QUARANTINE_COMMENT_MARKER))
    {
        report.skip(n, Skip::DaemonHold);
        return false;
    }
    let mark = marker(plan.releases(), &plan.resolved);
    let already = trusted.iter().any(|b| b.contains(&mark));

    let restored = if plan.releases() {
        match restore_label(extra, plan.kind, n) {
            Ok(r) => r,
            Err(e) => {
                report.unread(n, format!("label-event read failed: {e}"));
                return false;
            }
        }
    } else {
        None
    };
    let mut acted = Acted {
        kind: plan.kind.label(),
        number: n,
        resolved: plan.resolved.clone(),
        still_open: plan.open.clone(),
        restored: restored.clone(),
        commented: false,
        applied: false,
    };
    if !dry_run {
        match write(park, extra, plan, &fresh, restored.as_deref(), already) {
            Ok(commented) => {
                acted.commented = commented;
                acted.applied = true;
            }
            Err(e) => {
                report.fail(n, e);
                return true;
            }
        }
    }
    report.acted(plan.releases(), acted);
    true
}

/// The lane label a release restores: an issue gets `loom:issue` only if it
/// ever carried it (Guide's `was_previously_approved`); a PR gets
/// `loom:changes-requested` when that was its latest review label, else
/// `loom:review-requested` (`park-record.md`'s `previous_review_label`).
fn restore_label(
    extra: &mut dyn ReleaseForge,
    kind: Artifact,
    n: u64,
) -> Result<Option<String>, String> {
    let events = extra.labeled_events(n)?;
    Ok(match kind {
        Artifact::Issue => events
            .iter()
            .any(|l| l == ISSUE_LABEL)
            .then(|| ISSUE_LABEL.to_string()),
        Artifact::Pr => {
            let last = events
                .iter()
                .rev()
                .find(|l| *l == REVIEW_REQUESTED || *l == CHANGES_REQUESTED);
            Some(if last.is_some_and(|l| l == CHANGES_REQUESTED) {
                CHANGES_REQUESTED.to_string()
            } else {
                REVIEW_REQUESTED.to_string()
            })
        }
    })
}

/// The writes, in order: comment (unless already posted), then the edit.
/// Returns whether a comment was posted.
fn write(
    park: &mut dyn ParkForge,
    extra: &mut dyn ReleaseForge,
    plan: &Plan,
    fresh: &crate::operator_decision::cli::IssueState,
    restored: Option<&str>,
    already: bool,
) -> Result<bool, String> {
    let n = u64::from(plan.row.number);
    if !already {
        extra
            .post_comment(n, plan.kind == Artifact::Pr, &comment_body(plan, restored))
            .map_err(|e| format!("audit comment failed, nothing edited: {e}"))?;
    }
    if plan.releases() {
        if let Some(label) = restored {
            if !fresh.labels.iter().any(|l| l == label) {
                park.add_labels(n, &[label.to_string()])
                    .map_err(|e| format!("restoring {label} failed: {e}"))?;
            }
        }
        park.remove_label(n, BLOCKED_LABEL)
            .map_err(|e| format!("removing {BLOCKED_LABEL} failed: {e}"))?;
    } else {
        park.set_body(n, &drop_blockers(&fresh.body, &plan.resolved))
            .map_err(|e| format!("re-park body write failed: {e}"))?;
    }
    Ok(!already)
}
