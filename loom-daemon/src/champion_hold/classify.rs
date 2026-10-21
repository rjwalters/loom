//! What Champion does with a PR's verdict and park labels (#11932): the
//! Verdict-State Janitor's exception for Champion holds, in code.
//!
//! # Why this is not prose
//!
//! After #11861 a `loom:pr` + `loom:blocked` pair is legal only as Champion's
//! own hold, keyed on a standing trusted hold marker ([`super::standing_hold`]).
//! The daemon read that marker, but the janitor's side of the rule lived only
//! in `champion-pr-merge.md`. A pair with **no** marker (hand-moved, or parked
//! by a sweep) then fell through: Champion could not release a hold it never
//! wrote, `merge-pr.sh` refused the merge, and the Error Handling section posted
//! an unguarded "Merge Failed" comment on every pass.
//!
//! [`classify`] makes the decision once, from labels plus trusted comments:
//!
//! | state | labels | Champion |
//! |---|---|---|
//! | [`PrClass::ForeignPark`] | `loom:blocked`, no standing hold marker | skips the PR: no merge attempt, no comment, no label change |
//! | [`PrClass::Contradiction`] | `loom:pr` beside `loom:changes-requested` / `loom:review-requested` | the janitor repairs it (strips `loom:pr`) |
//! | [`PrClass::ChampionHold`] | `loom:blocked` with a standing hold marker | leaves it; the `merge-risk` and `critical-files` criteria decide |
//! | [`PrClass::StrayOperator`] | a hold, plus a fleet-added `loom:operator` and no capped-hold recommendation | removes `loom:operator`; the hold stands |
//! | [`PrClass::StackedOnLoomBranch`] | otherwise clear, but based on a Loom stack branch | skips the PR: no merge attempt, no comment, no label change |
//! | [`PrClass::Clear`] | none of these | the 6 criteria run |
//!
//! A foreign park is checked first: a PR another actor parked is theirs, even
//! when its verdict labels also contradict. A contradiction beside a Champion
//! hold (two Judges racing on a held PR) is still repaired, and the hold
//! survives the repair because `loom:blocked` is never touched.
//!
//! A stray operator (#11970) is a hold that a pre-#11888 Champion prompt
//! re-labelled `loom:operator` (its old LABEL_REPAIR): every role skips that
//! label, so the approved hold is stranded. Only the fleet's own add is
//! stray: a human's `loom:operator`, and the capped-hold inbox route (#11905),
//! are holds for a person and stay ([`classify_operator`]).
//!
//! A stacked child (#12761) targets its parent's head branch, not the default
//! branch. Merging it there moves the parent PR's head under its approval (or,
//! once the parent is gone, lands it on a dead branch), and its
//! `Closes #<child>` never fires (a non-default-branch merge). The merge
//! sequencer's `StackedBase` edge guards it only above `TRIGGER_OPEN_PRS` open
//! PRs, and only after a claim-reconcile tick. So a PR that would otherwise be
//! [`PrClass::Clear`] is [`PrClass::StackedOnLoomBranch`] whenever its base is
//! a [stackable](crate::merge_pr::stacked_children::is_stackable_parent_branch)
//! Loom branch (`feature/issue-<N>`, `feature/harness-ops-<N>`), whatever the
//! state of the parent PR that heads it ([`stacked_base`],
//! [`with_stacked_base`]). The parent is deliberately not looked up: an open
//! parent, a merged parent whose branch survived, and a parent closed unmerged
//! are all a merge Champion must not make. Liveness holds without the lookup:
//! when the parent lands, GitHub (branch deleted) or the merged-parent
//! reconcile (branch kept) retargets the child to the default branch, and it
//! is clear again. A closed parent's orphan stays skipped until #12760's pass
//! or a human retargets it. Any other base (the default branch, `release/*`,
//! `develop`, ...) is untouched: a deliberate non-default base still merges. A
//! hold, park or contradiction keeps its own state: none of them merges
//! either, and each has its own handling.
//!
//! [`merge_failed_marker`] is the second half: the "Merge Failed" notice is
//! posted at most once per head.

use serde_json::Value;

use super::{capped_hold_routed, standing_hold, HoldKind};
use crate::work_finder::OPERATOR_HOLD_LABEL;

/// The first token of every `champion-hold classify` stdout line.
pub const CLASSIFY_SENTINEL: &str = "LOOM-CHAMPION-HOLD";

/// The verdict labels that contradict `loom:pr` for the janitor, in the order
/// it reports them (#4570, #7018).
pub const CONTRADICTING_VERDICTS: [&str; 2] = ["loom:changes-requested", "loom:review-requested"];

/// The "Merge Failed" notice's marker prefix; the full marker names the head
/// (`<!-- champion:merge-failed head=<sha> -->`).
pub const MERGE_FAILED_MARKER_PREFIX: &str = "<!-- champion:merge-failed head=";

/// What Champion does with a PR (see the module table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrClass {
    /// Nothing for the janitor: the 6 criteria run.
    Clear,
    /// Champion's own hold: leave it.
    ChampionHold(HoldKind),
    /// `loom:blocked` from another actor: skip, silently.
    ForeignPark,
    /// `loom:pr` beside the named verdict label: strip `loom:pr`.
    Contradiction(&'static str),
    /// Champion's own hold carrying a fleet-added `loom:operator` (#11970):
    /// remove `loom:operator`, leave the hold.
    StrayOperator(HoldKind),
    /// Based on the named Loom stack branch (#12761), whether or not an open
    /// PR still heads it: skip, silently, until this is retargeted.
    StackedOnLoomBranch(String),
}

impl PrClass {
    /// The `state=` token.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self {
            PrClass::Clear => "clear",
            PrClass::ChampionHold(_) => "champion-hold",
            PrClass::ForeignPark => "foreign-park",
            PrClass::Contradiction(_) => "contradiction",
            PrClass::StrayOperator(_) => "stray-operator",
            PrClass::StackedOnLoomBranch(_) => "stacked-on-loom-branch",
        }
    }

    /// The stdout line: `LOOM-CHAMPION-HOLD state=<s> pr=<N> detail=<d>`.
    /// `detail` is the contradicting label, the hold kind, the stacked base
    /// branch, or `-`.
    #[must_use]
    pub fn line(&self, pr: u64) -> String {
        let detail = match self {
            PrClass::Contradiction(label) => *label,
            PrClass::StackedOnLoomBranch(base) => base.as_str(),
            PrClass::ChampionHold(HoldKind::MergeRisk)
            | PrClass::StrayOperator(HoldKind::MergeRisk) => "merge-risk",
            PrClass::ChampionHold(HoldKind::CriticalFile)
            | PrClass::StrayOperator(HoldKind::CriticalFile) => "critical-file",
            PrClass::Clear | PrClass::ForeignPark => "-",
        };
        format!("{CLASSIFY_SENTINEL} state={} pr={pr} detail={detail}", self.state())
    }
}

/// Whether the trusted comments are needed to classify `labels`: only a
/// `loom:blocked` PR asks whose park it is.
#[must_use]
pub fn needs_comments<L: AsRef<str>>(labels: &[L]) -> bool {
    crate::work_finder::is_blocked_hold(labels)
}

/// Classify a PR from its labels and its **trusted** comment bodies, oldest
/// first (`comment_trust`, #9548). `trusted` is read only when
/// [`needs_comments`]; a caller that could not read it must not call this
/// for such a PR (an unknown owner is not a foreign park, nor a hold).
#[must_use]
pub fn classify<L: AsRef<str>, C: AsRef<str>>(labels: &[L], trusted: &[C]) -> PrClass {
    let has = |want: &str| labels.iter().any(|l| l.as_ref() == want);
    let hold = if needs_comments(labels) {
        match standing_hold(trusted) {
            Some(kind) => Some(kind),
            None => return PrClass::ForeignPark,
        }
    } else {
        None
    };
    if has("loom:pr") {
        if let Some(label) = CONTRADICTING_VERDICTS.iter().copied().find(|l| has(l)) {
            return PrClass::Contradiction(label);
        }
    }
    hold.map_or(PrClass::Clear, PrClass::ChampionHold)
}

/// Who added the `loom:operator` standing on a PR (#11970).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorAdder {
    /// The fleet's own GitHub App: a role, i.e. a stale Champion prompt.
    Fleet,
    /// Anyone else: a human (or another App), whose hold it is.
    Other,
}

/// The Capped-PR Recovery Pass's close recommendation, which also adds
/// `loom:operator` for a human (`champion-pr-merge.md`).
pub const CAPPED_CLOSE_RECOMMENDED_MARKER: &str = "<!-- champion:capped-pr-close-recommended -->";

/// Whether a stray `loom:operator` is possible here, so the label events are
/// worth reading: `loom:operator` beside a standing trusted Champion hold
/// (`loom:blocked` + marker) with no capped-hold recommendation after the
/// marker. That recommendation's `loom:operator` is the operator-inbox route
/// (#11905), never stray; a close recommendation after the hold is treated the
/// same way.
#[must_use]
pub fn stray_operator_candidate<L: AsRef<str>, C: AsRef<str>>(labels: &[L], trusted: &[C]) -> bool {
    let hold = trusted.iter().rposition(|c| {
        let c = c.as_ref();
        c.starts_with(super::MERGE_RISK_HOLD_MARKER)
            || c.starts_with(super::CRITICAL_FILE_HOLD_MARKER)
    });
    let closing = trusted.iter().enumerate().any(|(i, c)| {
        hold.is_none_or(|h| i > h) && c.as_ref().starts_with(CAPPED_CLOSE_RECOMMENDED_MARKER)
    });
    labels.iter().any(|l| l.as_ref() == OPERATOR_HOLD_LABEL)
        && crate::work_finder::is_blocked_hold(labels)
        && standing_hold(trusted).is_some()
        && !capped_hold_routed(trusted)
        && !closing
}

/// Who added the `loom:operator` the PR carries now, from its REST issue
/// events (oldest first): the actor of the newest `labeled` event for it,
/// [`OperatorAdder::Fleet`] when `is_fleet` claims the login. `None` when no
/// such event names an actor: unknown is never the fleet.
#[must_use]
pub fn operator_adder(events: &[Value], is_fleet: impl Fn(&str) -> bool) -> Option<OperatorAdder> {
    let event = events.iter().rev().find(|e| {
        e.get("event").and_then(Value::as_str) == Some("labeled")
            && e.pointer("/label/name").and_then(Value::as_str) == Some(OPERATOR_HOLD_LABEL)
    })?;
    let login = event
        .pointer("/actor/login")
        .and_then(Value::as_str)?
        .trim();
    if login.is_empty() {
        return None;
    }
    Some(if is_fleet(login) {
        OperatorAdder::Fleet
    } else {
        OperatorAdder::Other
    })
}

/// Who added the PR's standing `loom:operator` (#11970), read live from its
/// REST issue events through the `gh` facade (every page; counted per page
/// when the page walk is on), with the fleet's logins
/// ([`crate::forge_identity::FleetLogins`]) as [`OperatorAdder::Fleet`].
/// `repo` is `owner/repo` or the `{owner}/{repo}` placeholder. `None` when the
/// read failed or no event names an actor.
#[must_use]
pub fn fetch_operator_adder(repo: &str, pr: u64, root: &std::path::Path) -> Option<OperatorAdder> {
    use crate::claim_reconciliation::gh_call;
    let path = format!("repos/{repo}/issues/{pr}/events?per_page=100");
    let gh = std::path::Path::new("gh");
    let out = gh_call::ok_stdout(gh_call::read("champion.label_events", gh, root).args([
        "api",
        &path,
        "--paginate",
    ]))?;
    let events = crate::comment_trust::parse_listing(&out)?;
    let fleet = crate::forge_identity::FleetLogins::for_root(root);
    operator_adder(&events, |login| fleet.contains(login))
}

/// [`classify`], then (#11970) a Champion hold whose `loom:operator` the fleet
/// added, with no capped-hold recommendation standing, is
/// [`PrClass::StrayOperator`]. `adder` is `None` when the events were not read
/// or named no actor: the hold is left as it is.
#[must_use]
pub fn classify_operator<L: AsRef<str>, C: AsRef<str>>(
    labels: &[L],
    trusted: &[C],
    adder: Option<OperatorAdder>,
) -> PrClass {
    match classify(labels, trusted) {
        PrClass::ChampionHold(kind)
            if adder == Some(OperatorAdder::Fleet) && stray_operator_candidate(labels, trusted) =>
        {
            PrClass::StrayOperator(kind)
        }
        class => class,
    }
}

/// The PR's Loom stack base from its REST pull object
/// (`GET repos/{o}/{r}/pulls/{n}`, #12761): `Some(Some(branch))` for a
/// [stackable](crate::merge_pr::stacked_children::is_stackable_parent_branch)
/// base, `Some(None)` for any other (the default branch, `release/*`, ...).
/// `None` when `base.ref` is missing or empty: an unknown base is never an
/// unstacked one.
#[must_use]
pub fn stacked_base(pull: &Value) -> Option<Option<String>> {
    let branch = pull.pointer("/base/ref").and_then(Value::as_str)?.trim();
    if branch.is_empty() {
        return None;
    }
    Some(
        crate::merge_pr::stacked_children::is_stackable_parent_branch(branch)
            .then(|| branch.to_string()),
    )
}

/// `class`, except that a [`PrClass::Clear`] PR based on the Loom stack
/// branch `stacked_base` is [`PrClass::StackedOnLoomBranch`]. Every other
/// state stands.
#[must_use]
pub fn with_stacked_base(class: PrClass, stacked_base: Option<&str>) -> PrClass {
    match (class, stacked_base) {
        (PrClass::Clear, Some(base)) => PrClass::StackedOnLoomBranch(base.to_string()),
        (class, _) => class,
    }
}

/// The marker a "Merge Failed" notice for `head` opens with.
#[must_use]
pub fn merge_failed_marker(head: &str) -> String {
    format!("{MERGE_FAILED_MARKER_PREFIX}{head} -->")
}

/// The marker to post a "Merge Failed" notice under, or `None` when a trusted
/// notice for this head already stands (once per head, #11932). `startswith`,
/// not `contains`: a later comment quoting the marker is not the notice
/// (#5371). An empty or non-hex head never posts.
#[must_use]
pub fn merge_failed_notice_due<C: AsRef<str>>(trusted: &[C], head: &str) -> Option<String> {
    if head.is_empty() || !head.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let marker = merge_failed_marker(head);
    (!trusted.iter().any(|c| c.as_ref().starts_with(&marker))).then_some(marker)
}

#[cfg(test)]
mod tests {
    use super::super::{
        CAPPED_HOLD_RECOMMENDATION_MARKER, CRITICAL_FILE_HOLD_MARKER, MERGE_RISK_CLEARED_MARKER,
        MERGE_RISK_HOLD_MARKER,
    };
    use super::*;

    const NONE: [&str; 0] = [];

    fn hold() -> String {
        format!("{MERGE_RISK_HOLD_MARKER}\n<!-- champion:hold-state head=abc -->")
    }

    const STRANDED: [&str; 3] = ["loom:pr", "loom:blocked", "loom:operator"];

    fn fleet(login: &str) -> bool {
        login == "loom-fleet-dispatch[bot]"
    }

    fn labeled(name: &str, login: &str) -> Value {
        serde_json::json!({"event": "labeled", "label": {"name": name}, "actor": {"login": login}})
    }

    #[test]
    fn a_fleet_added_operator_on_a_hold_is_stray() {
        assert_eq!(
            classify_operator(&STRANDED, &[hold()], Some(OperatorAdder::Fleet)),
            PrClass::StrayOperator(HoldKind::MergeRisk)
        );
        // A hold out at Doctor is stranded the same way.
        let doctor = ["loom:changes-requested", "loom:blocked", "loom:operator"];
        assert_eq!(
            classify_operator(&doctor, &[hold()], Some(OperatorAdder::Fleet)),
            PrClass::StrayOperator(HoldKind::MergeRisk)
        );
        let cf = format!("{CRITICAL_FILE_HOLD_MARKER}\nheld");
        assert_eq!(
            classify_operator(&STRANDED, &[cf], Some(OperatorAdder::Fleet)).line(7),
            "LOOM-CHAMPION-HOLD state=stray-operator pr=7 detail=critical-file"
        );
    }

    #[test]
    fn a_human_added_operator_is_left_alone() {
        for adder in [Some(OperatorAdder::Other), None] {
            assert_eq!(
                classify_operator(&STRANDED, &[hold()], adder),
                PrClass::ChampionHold(HoldKind::MergeRisk),
                "{adder:?}"
            );
        }
    }

    #[test]
    fn the_capped_hold_route_is_left_alone() {
        let rec = format!("{CAPPED_HOLD_RECOMMENDATION_MARKER}\nreview, close or release");
        let trusted = [hold(), rec.clone()];
        assert!(!stray_operator_candidate(&STRANDED, &trusted));
        assert_eq!(
            classify_operator(&STRANDED, &trusted, Some(OperatorAdder::Fleet)),
            PrClass::ChampionHold(HoldKind::MergeRisk)
        );
        // A recommendation from an earlier hold episode routes nothing now.
        assert!(stray_operator_candidate(&STRANDED, &[rec, hold()]));
        // Nor is a close recommendation's loom:operator stray.
        let close = format!("{CAPPED_CLOSE_RECOMMENDED_MARKER}\nrecommend closing");
        assert!(!stray_operator_candidate(&STRANDED, &[hold(), close]));
    }

    #[test]
    fn no_hold_or_no_operator_is_never_stray() {
        let f = Some(OperatorAdder::Fleet);
        // No standing marker: another actor's park, whoever added the label.
        assert_eq!(classify_operator(&STRANDED, &NONE, f), PrClass::ForeignPark);
        // No loom:blocked: the pre-#11861 shape, the migration's to move.
        assert_eq!(classify_operator(&["loom:pr", "loom:operator"], &[hold()], f), PrClass::Clear);
        // No loom:operator: just the hold.
        assert_eq!(
            classify_operator(&["loom:pr", "loom:blocked"], &[hold()], f),
            PrClass::ChampionHold(HoldKind::MergeRisk)
        );
        // A contradiction is repaired first.
        let both = [
            "loom:pr",
            "loom:review-requested",
            "loom:blocked",
            "loom:operator",
        ];
        assert_eq!(
            classify_operator(&both, &[hold()], f),
            PrClass::Contradiction("loom:review-requested")
        );
    }

    #[test]
    fn the_adder_is_the_newest_operator_labeled_event() {
        let bot = "loom-fleet-dispatch[bot]";
        let events = [
            labeled("loom:operator", "a-human"),
            serde_json::json!({"event": "unlabeled", "label": {"name": "loom:operator"}, "actor": {"login": "a-human"}}),
            labeled("loom:operator", bot),
            labeled("loom:blocked", "a-human"),
        ];
        assert_eq!(operator_adder(&events, fleet), Some(OperatorAdder::Fleet));
        let events = [
            labeled("loom:operator", bot),
            labeled("loom:operator", "a-human"),
        ];
        assert_eq!(operator_adder(&events, fleet), Some(OperatorAdder::Other));
        // Another App is not the fleet.
        let events = [labeled("loom:operator", "some-other-app[bot]")];
        assert_eq!(operator_adder(&events, fleet), Some(OperatorAdder::Other));
        // No event, or no actor (a deleted account): unknown, never the fleet.
        assert_eq!(operator_adder(&[labeled("loom:blocked", bot)], fleet), None);
        let ghost = [
            serde_json::json!({"event": "labeled", "label": {"name": "loom:operator"}, "actor": null}),
        ];
        assert_eq!(operator_adder(&ghost, fleet), None);
    }

    #[test]
    fn a_markerless_approved_park_is_foreign() {
        assert_eq!(classify(&["loom:pr", "loom:blocked"], &NONE), PrClass::ForeignPark);
        // A quoted marker is prose, not a hold.
        let quoted = format!("see {MERGE_RISK_HOLD_MARKER}");
        assert_eq!(classify(&["loom:pr", "loom:blocked"], &[quoted]), PrClass::ForeignPark);
        // A released hold leaves the park ownerless, i.e. not Champion's.
        let released = format!("Merging.\n{MERGE_RISK_CLEARED_MARKER}");
        assert_eq!(
            classify(&["loom:pr", "loom:blocked"], &[hold(), released]),
            PrClass::ForeignPark
        );
    }

    #[test]
    fn a_foreign_park_wins_over_a_verdict_contradiction() {
        let labels = ["loom:pr", "loom:changes-requested", "loom:blocked"];
        assert_eq!(classify(&labels, &NONE), PrClass::ForeignPark);
    }

    #[test]
    fn a_standing_marker_makes_it_champions_hold() {
        assert_eq!(
            classify(&["loom:pr", "loom:blocked"], &[hold()]),
            PrClass::ChampionHold(HoldKind::MergeRisk)
        );
        let cf = format!("{CRITICAL_FILE_HOLD_MARKER}\nheld");
        assert_eq!(
            classify(&["loom:pr", "loom:blocked"], &[cf]),
            PrClass::ChampionHold(HoldKind::CriticalFile)
        );
        // The Doctor round trip of a hold is still the hold.
        assert_eq!(
            classify(&["loom:changes-requested", "loom:blocked"], &[hold()]),
            PrClass::ChampionHold(HoldKind::MergeRisk)
        );
    }

    #[test]
    fn verdict_contradictions_are_repaired_even_on_a_hold() {
        for other in CONTRADICTING_VERDICTS {
            assert_eq!(classify(&["loom:pr", other], &NONE), PrClass::Contradiction(other));
            assert_eq!(
                classify(&["loom:pr", other, "loom:blocked"], &[hold()]),
                PrClass::Contradiction(other),
                "a Judge race on a held PR"
            );
        }
        // Both: the fixed report order, regardless of label order.
        let both = ["loom:review-requested", "loom:pr", "loom:changes-requested"];
        assert_eq!(classify(&both, &NONE), PrClass::Contradiction("loom:changes-requested"));
    }

    #[test]
    fn anything_else_is_clear() {
        assert_eq!(classify(&["loom:pr"], &NONE), PrClass::Clear);
        assert_eq!(classify(&["loom:review-requested"], &NONE), PrClass::Clear);
        assert_eq!(classify::<&str, &str>(&[], &[]), PrClass::Clear);
        // Out of scope here: merge-pr.sh and the criteria own these parks.
        assert_eq!(classify(&["loom:pr", "loom:operator"], &NONE), PrClass::Clear);
    }

    #[test]
    fn comments_are_needed_only_for_a_blocked_pr() {
        assert!(needs_comments(&["loom:pr", "loom:blocked"]));
        assert!(!needs_comments(&["loom:pr", "loom:changes-requested"]));
    }

    #[test]
    fn the_line_names_state_pr_and_detail() {
        assert_eq!(PrClass::Clear.line(7), "LOOM-CHAMPION-HOLD state=clear pr=7 detail=-");
        assert_eq!(
            PrClass::ForeignPark.line(7),
            "LOOM-CHAMPION-HOLD state=foreign-park pr=7 detail=-"
        );
        assert_eq!(
            PrClass::Contradiction("loom:review-requested").line(7),
            "LOOM-CHAMPION-HOLD state=contradiction pr=7 detail=loom:review-requested"
        );
        assert_eq!(
            PrClass::ChampionHold(HoldKind::CriticalFile).line(7),
            "LOOM-CHAMPION-HOLD state=champion-hold pr=7 detail=critical-file"
        );
    }

    fn pull(base: &str) -> Value {
        serde_json::json!({
            "number": 2,
            "base": {"ref": base, "label": format!("acme:{base}"), "repo": {"owner": {"login": "acme"}}},
        })
    }

    /// The CLI's composition, over canned REST reads: labels and the PR's
    /// pull object (#12761). No parent lookup.
    fn classify_stacked(labels: &[&str], pull: &Value) -> Option<PrClass> {
        let class = classify(labels, &NONE);
        Some(with_stacked_base(class, stacked_base(pull)?.as_deref()))
    }

    #[test]
    fn an_approved_child_of_an_open_parent_is_not_clear() {
        // Two open PRs: parent #1 (feature/issue-1 -> main) and its approved
        // child #2 (-> feature/issue-1). The sequencer does not run at <= 2
        // open PRs, so this state is the only thing between #2 and a merge.
        let class = classify_stacked(&["loom:pr"], &pull("feature/issue-1"));
        assert_eq!(class, Some(PrClass::StackedOnLoomBranch("feature/issue-1".into())));
        assert_eq!(
            class.unwrap().line(2),
            "LOOM-CHAMPION-HOLD state=stacked-on-loom-branch pr=2 detail=feature/issue-1"
        );
        assert_eq!(
            classify_stacked(&["loom:pr"], &pull("feature/harness-ops-350")),
            Some(PrClass::StackedOnLoomBranch("feature/harness-ops-350".into()))
        );
    }

    #[test]
    fn a_default_or_non_loom_base_is_unaffected() {
        for base in [
            "main",
            "release/1.2",
            "develop",
            "feature/issue-1-extra",
            "feature/other-3",
        ] {
            assert_eq!(stacked_base(&pull(base)), Some(None), "{base}");
            assert_eq!(classify_stacked(&["loom:pr"], &pull(base)), Some(PrClass::Clear), "{base}");
        }
    }

    #[test]
    fn a_landed_parent_does_not_clear_the_child() {
        // A merged parent whose branch survived: the base is still a Loom
        // stack branch, so the child stays skipped until it is retargeted
        // (a merge here would land on a branch that already landed).
        assert_eq!(
            classify_stacked(&["loom:pr"], &pull("feature/issue-1")),
            Some(PrClass::StackedOnLoomBranch("feature/issue-1".into()))
        );
    }

    #[test]
    fn a_closed_unmerged_parent_does_not_clear_the_child() {
        // The parent was closed and its branch kept: the decision reads only
        // the child's base, so a dead parent is skipped exactly like a live one.
        assert_eq!(
            classify_stacked(&["loom:pr"], &pull("feature/harness-ops-7")),
            Some(PrClass::StackedOnLoomBranch("feature/harness-ops-7".into()))
        );
    }

    #[test]
    fn an_unreadable_base_is_never_clear() {
        assert_eq!(stacked_base(&serde_json::json!({"number": 2})), None);
        assert_eq!(stacked_base(&serde_json::json!({"base": {"ref": ""}})), None);
        assert_eq!(stacked_base(&serde_json::json!({"message": "Bad credentials"})), None);
        let unread = serde_json::json!({"message": "Bad credentials"});
        assert_eq!(classify_stacked(&["loom:pr"], &unread), None);
        // No owner is needed any more: there is no head= lookup to build.
        assert_eq!(
            stacked_base(&serde_json::json!({"base": {"ref": "feature/issue-1"}})),
            Some(Some("feature/issue-1".into()))
        );
    }

    #[test]
    fn a_hold_park_or_contradiction_keeps_its_own_state() {
        let base = Some("feature/issue-1");
        for class in [
            PrClass::ForeignPark,
            PrClass::ChampionHold(HoldKind::MergeRisk),
            PrClass::StrayOperator(HoldKind::CriticalFile),
            PrClass::Contradiction("loom:changes-requested"),
        ] {
            assert_eq!(with_stacked_base(class.clone(), base), class);
        }
        assert_eq!(with_stacked_base(PrClass::Clear, None), PrClass::Clear);
    }

    #[test]
    fn the_merge_failed_notice_posts_once_per_head() {
        let first = merge_failed_notice_due(&NONE, "abc123").expect("first failure posts");
        assert_eq!(first, "<!-- champion:merge-failed head=abc123 -->");
        let posted = format!("{first}\n**Champion: Merge Failed**");
        assert_eq!(merge_failed_notice_due(&[posted.as_str()], "abc123"), None, "same head");
        assert!(merge_failed_notice_due(&[posted.as_str()], "def456").is_some(), "new head");
        // A quote of the marker is not the notice.
        let quoted = format!("Earlier: {first}");
        assert!(merge_failed_notice_due(&[quoted.as_str()], "abc123").is_some());
        // No head, or a head that is not a SHA: never post.
        assert_eq!(merge_failed_notice_due(&NONE, ""), None);
        assert_eq!(merge_failed_notice_due(&NONE, "abc -->"), None);
    }

    #[test]
    fn a_marker_for_a_longer_head_is_not_this_heads_notice() {
        let other = format!("{}\nx", merge_failed_marker("abc1234"));
        assert!(merge_failed_notice_due(&[other.as_str()], "abc123").is_some());
    }
}
