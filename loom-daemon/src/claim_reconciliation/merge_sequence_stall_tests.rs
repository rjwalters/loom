//! Fixtures for #10060: direct-overlap edges, level-first ordering, and the
//! stalled-head release + escalation.

use super::super::SEQUENCE_LABEL;
use super::*;

const NOW: &str = "2026-10-03T12:00:00Z";
const BOUND: f64 = 12.0;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(NOW)
        .unwrap()
        .with_timezone(&Utc)
}

fn sha(n: u32) -> String {
    format!("{n:040x}")
}

/// An open PR created `created`, last active `updated`.
fn pr(number: u32, created: &str, updated: &str, labels: &[&str]) -> SequencePr {
    SequencePr {
        number,
        created_at: created.to_string(),
        updated_at: updated.to_string(),
        head_sha: Some(sha(number)),
        head_ref: format!("feature/issue-{number}"),
        base_ref: "main".to_string(),
        draft: false,
        labels: labels.iter().map(|l| (*l).to_string()).collect(),
    }
}

// --- Slice 3: stalled heads ---------------------------------------------

fn marker(after: u32, source: Option<&str>, plan: &str) -> SequenceMarker {
    SequenceMarker {
        after,
        pred_head: sha(after),
        follower_head: sha(99),
        plan: plan.into(),
        source: source.map(str::to_string),
    }
}

/// The predecessor read from the pulls API: open at its recorded head.
fn pred(n: u32, updated: Option<&str>) -> PredecessorState {
    PredecessorState {
        open: true,
        merged: false,
        head_sha: Some(sha(n)),
        updated_at: updated.map(str::to_string),
    }
}

/// Phase 1's decision for a follower (approved) behind `head`.
fn decide(m: &SequenceMarker, head: &SequencePr, approved: bool) -> HoldAction {
    let cause = stall_cause(head, now(), BOUND);
    // The pulls read is "now" fresh so the 72 h expiry never fires here.
    let p = pred(head.number, Some(&Utc::now().to_rfc3339()));
    hold_action_with_stall(m, Some(&p), Some(&m.follower_head), approved, 72.0, cause.as_ref())
}

#[test]
fn a_human_held_head_past_the_bound_releases_soft_holds_only() {
    // (a) #9745-shaped: the head carries `loom:operator`, quiet 2 days.
    let head = pr(5, "2026-09-30T00:00:00Z", "2026-10-01T00:00:00Z", &["loom:pr", "loom:operator"]);
    assert_eq!(
        stall_cause(&head, now(), BOUND),
        Some(StallCause::HumanHold("loom:operator".into()))
    );
    let soft = marker(5, Some("pass"), "seq-a");
    assert_eq!(decide(&soft, &head, true), HoldAction::ReleaseStalled);
    // Not approved ⇒ nothing mergeable is starved: hold.
    assert_eq!(decide(&soft, &head, false), HoldAction::HoldSoft);
    // Hard (human-authored, no source=) ⇒ never released.
    assert_eq!(decide(&marker(5, None, "manual"), &head, true), HoldAction::HoldHard);
    // Consolidation reservations keep ADR-0023's 72 h contract.
    assert_eq!(
        decide(&marker(5, Some("pass"), "cons-ab12cd34"), &head, true),
        HoldAction::HoldSoft
    );
    // Within the bound ⇒ hold.
    let recent = pr(5, "2026-09-30T00:00:00Z", "2026-10-03T06:00:00Z", &["loom:operator"]);
    assert_eq!(stall_cause(&recent, now(), BOUND), None);
    assert_eq!(decide(&soft, &recent, true), HoldAction::HoldSoft);
}

#[test]
fn a_no_verdict_head_past_the_bound_releases_soft_holds() {
    // (b) #9819-shaped: a fork PR whose CI is `action_required`, so it never
    // gets a verdict — `loom:review-requested`, no `loom:pr`.
    let head = pr(7, "2026-09-30T00:00:00Z", "2026-10-02T00:00:00Z", &["loom:review-requested"]);
    assert_eq!(stall_cause(&head, now(), BOUND), Some(StallCause::NoVerdict));
    let soft = marker(7, Some("pass"), "seq-b");
    assert_eq!(decide(&soft, &head, true), HoldAction::ReleaseStalled);
    assert_eq!(decide(&marker(7, None, "manual"), &head, true), HoldAction::HoldHard);
    // Within the bound ⇒ HoldSoft.
    let recent = pr(7, "2026-09-30T00:00:00Z", "2026-10-03T08:00:00Z", &["loom:review-requested"]);
    assert_eq!(decide(&soft, &recent, true), HoldAction::HoldSoft);
    // An approved, quiet head is not stalled: Champion can merge it.
    let approved = pr(7, "2026-09-30T00:00:00Z", "2026-10-02T00:00:00Z", &["loom:pr"]);
    assert_eq!(decide(&soft, &approved, true), HoldAction::HoldSoft);
    // Unparseable freshness is never stalled.
    let unknown = pr(7, "2026-09-30T00:00:00Z", "yesterday", &[]);
    assert_eq!(stall_cause(&unknown, now(), BOUND), None);
}

#[test]
fn an_unreadable_or_moved_predecessor_is_never_stall_released() {
    let head = pr(7, "2026-09-30T00:00:00Z", "2026-10-02T00:00:00Z", &[]);
    let cause = stall_cause(&head, now(), BOUND);
    let soft = marker(7, Some("pass"), "seq-b");
    let live = Some(soft.follower_head.as_str());
    assert_eq!(
        hold_action_with_stall(&soft, None, live, true, 72.0, cause.as_ref()),
        HoldAction::HoldSoft,
        "fail closed"
    );
    let moved = PredecessorState {
        head_sha: Some(sha(777)),
        ..pred(7, None)
    };
    assert_eq!(
        hold_action_with_stall(&soft, Some(&moved), live, true, 72.0, cause.as_ref()),
        HoldAction::VoidAndReplan
    );
}

#[test]
fn a_stalled_chain_escalates_once_naming_head_action_and_held_prs() {
    let head = pr(5, "2026-09-30T00:00:00Z", "2026-10-01T00:00:00Z", &["loom:operator"]);
    let cause = stall_cause(&head, now(), BOUND).unwrap();
    let mut ledger = StallLedger::default();
    ledger.record(&head, &cause, 36.0, 11, true);
    ledger.record(&head, &cause, 36.0, 12, false);
    ledger.record(&head, &cause, 36.0, 12, false);
    assert_eq!(ledger.chains.len(), 1, "one chain per head");
    let chain = &ledger.chains[0];
    assert_eq!((chain.released.clone(), chain.held.clone()), (vec![11], vec![12]));
    assert!(chain.needs_escalation());
    let body = chain.comment_body(BOUND);
    assert!(body.starts_with(&escalation_marker(&chain.key())), "{body}");
    assert!(
        !body.contains("operator-priority-escalation"),
        "no star-liveness marker (#11104)"
    );
    assert!(body.contains("**Operator needed**"), "{body}");
    assert!(body.contains("decide #5"), "{body}");
    assert!(body.contains("#12") && body.contains("#11"), "{body}");
    assert!(body.contains("36h"), "{body}");
    assert!(chain.key().starts_with("sequence-stall:5:"));

    // A no-verdict head whose soft followers were all released is re-planned
    // rather than escalated (a comment would reset its quiet clock).
    let fork = pr(7, "2026-09-30T00:00:00Z", "2026-10-02T00:00:00Z", &[]);
    let mut l2 = StallLedger::default();
    l2.record(&fork, &StallCause::NoVerdict, 36.0, 8, true);
    assert!(!l2.chains[0].needs_escalation());
    l2.record(&fork, &StallCause::NoVerdict, 36.0, 9, false);
    assert!(l2.chains[0].needs_escalation(), "a hard-held follower still escalates");
    assert!(l2.chains[0].comment_body(BOUND).contains("action_required"));
}

#[cfg(unix)]
fn fake_gh(dir: &std::path::Path, marker: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let posted = dir.join("posted.flag");
    let log = dir.join("gh.log");
    let bin = dir.join("fake-gh-stall.sh");
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$1 $2\" >> \"{log}\"\n\
         if [ \"$1\" = api ]; then\n  if [ -f \"{posted}\" ]; then\n    \
         echo '[{{\"body\":\"{marker}\",\"author_association\":\"OWNER\",\"user\":{{\"login\":\"op\",\"type\":\"User\"}}}}]'\n  \
         else echo '[]'; fi\nelif [ \"$1 $2\" = \"pr comment\" ]; then touch \"{posted}\"; fi\n",
        log = log.display(),
        posted = posted.display(),
    );
    std::fs::write(&bin, script).unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
    (bin, log)
}

#[cfg(unix)]
#[test]
#[serial_test::serial]
fn the_second_tick_posts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let head = pr(5, "2026-09-30T00:00:00Z", "2026-10-01T00:00:00Z", &["loom:operator"]);
    let mut ledger = StallLedger::default();
    ledger.record(&head, &StallCause::HumanHold("loom:operator".into()), 36.0, 11, false);
    let chain = &ledger.chains[0];
    let (gh, log) = fake_gh(dir.path(), &escalation_marker(&chain.key()));
    assert!(escalate(&gh, &root, chain, BOUND).unwrap(), "first tick posts");
    assert!(!escalate(&gh, &root, chain, BOUND).unwrap(), "second tick: already_posted");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert_eq!(calls.matches("pr comment").count(), 1, "{calls}");
    // A held PR label alone never counts as an escalation.
    assert!(!already_escalated(&[SEQUENCE_LABEL.to_string()], chain));
}

/// #11104: a stall escalation posted before the marker rename (it carried the
/// removed star-liveness prefix) still counts as posted, so nothing re-posts.
#[test]
fn a_pre_rename_legacy_marker_still_dedupes() {
    let head = pr(5, "2026-09-30T00:00:00Z", "2026-10-01T00:00:00Z", &["loom:operator"]);
    let mut ledger = StallLedger::default();
    ledger.record(&head, &StallCause::HumanHold("loom:operator".into()), 36.0, 11, false);
    let chain = &ledger.chains[0];
    let legacy = format!("{LEGACY_MARKER_PREFIX}{} -->\nOperator needed", chain.key());
    assert!(already_escalated(&[legacy], chain));
    let other = format!("{LEGACY_MARKER_PREFIX}no-progress:issue-5 -->");
    assert!(!already_escalated(&[other], chain), "another key is not this chain's");
}

/// A fake `gh` for the #10089 dedupe test: `api` reads answer from the
/// `marker` / `fail` flag files; every call is logged as `"$1 $2"`.
#[cfg(unix)]
fn flagged_gh(dir: &std::path::Path, marker: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let log = dir.join("flagged.log");
    let bin = dir.join("fake-gh-flagged.sh");
    let script = format!(
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$1 $2\" >> \"{log}\"\n\
         if [ \"$1\" = api ]; then\n  [ -f \"{d}/fail\" ] && exit 1\n  \
         if [ -f \"{d}/marker\" ]; then\n    \
         echo '[{{\"body\":\"{marker}\",\"author_association\":\"OWNER\",\"user\":{{\"login\":\"op\",\"type\":\"User\"}}}}]'\n  \
         else echo '[]'; fi\nfi\n",
        log = log.display(),
        d = dir.display(),
    );
    std::fs::write(&bin, script).unwrap();
    let mut perms = std::fs::metadata(&bin).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&bin, perms).unwrap();
    (bin, log)
}

/// #10089: with the read cache on, "not yet escalated" is never cached (a
/// lagging listing must not suppress the first post), a failed read neither
/// posts nor caches, and a found marker stops further comment reads.
#[cfg(unix)]
#[test]
#[serial_test::serial]
fn escalation_dedupe_never_suppresses_the_first_post_and_never_reposts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let head = pr(5, "2026-09-30T00:00:00Z", "2026-10-01T00:00:00Z", &["loom:operator"]);
    let mut ledger = StallLedger::default();
    ledger.record(&head, &StallCause::HumanHold("loom:operator".into()), 36.0, 11, false);
    let chain = &ledger.chains[0];
    let (gh, log) = flagged_gh(dir.path(), &escalation_marker(&chain.key()));
    let count = |needle: &str| {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.contains(needle))
            .count()
    };
    let flag = |name: &str, on: bool| {
        let p = dir.path().join(name);
        if on {
            std::fs::write(p, "").unwrap();
        } else {
            let _ = std::fs::remove_file(p);
        }
    };

    crate::claim_reconciliation::read_cache::set_test_enabled(true);

    // (a) No marker: every call re-reads the comments and posts.
    for n in 1..=2 {
        assert!(escalate(&gh, &root, chain, BOUND).unwrap(), "call {n} posts");
        assert_eq!(count("api"), n, "call {n} re-reads comments");
        assert_eq!(count("pr comment"), n);
    }

    // (c) A failed read: Err, no post, nothing cached.
    flag("fail", true);
    assert!(escalate(&gh, &root, chain, BOUND).is_err());
    assert_eq!((count("api"), count("pr comment")), (3, 2), "failed read never posts");
    flag("fail", false);
    assert!(escalate(&gh, &root, chain, BOUND).unwrap(), "the failure cached nothing");
    assert_eq!((count("api"), count("pr comment")), (4, 3));

    // (b) Marker present: one read, then none; never a post.
    flag("marker", true);
    for _ in 0..4 {
        assert!(!escalate(&gh, &root, chain, BOUND).unwrap(), "already escalated");
    }
    assert_eq!(count("api"), 5, "one read, then served from cache");
    assert_eq!(count("pr comment"), 3, "no post once the marker is found");

    crate::claim_reconciliation::read_cache::set_test_enabled(false);
}
