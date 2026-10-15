//! Tests for the no-op hold (Issue #10156), driven through a fake `gh` whose
//! forge state is a handful of files the test can edit between no-ops.

use super::*;
use crate::peer_claims::{ClaimAd, PeerClaimView};
use crate::sweep_registry::test_support::fake_gh_graphql_arm;
use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

const ISSUE: u32 = 694;

struct Forge {
    ws: PathBuf,
    log: PathBuf,
}

impl Forge {
    fn set_issue(&self, n: u32, state: &str, body: &str, labels: &[&str]) {
        let labels: Vec<String> = labels.iter().map(|l| format!("\"{l}\"")).collect();
        let json = serde_json::json!({
            "number": n,
            "state": state,
            "body": body,
            "labels": labels.iter().map(|l| l.trim_matches('"')).collect::<Vec<_>>(),
        });
        std::fs::write(self.ws.join(format!("issue-{n}.json")), json.to_string()).unwrap();
    }
    /// Linked PRs as `(number, state, merged)`; each body closes [`ISSUE`].
    fn set_linked_prs(&self, prs: &[(u32, &str, bool)]) {
        let lines: String = prs
            .iter()
            .map(|(n, state, merged)| {
                format!(
                    "{{\"number\":{n},\"state\":\"{state}\",\"merged\":{merged},\
                     \"body\":\"Closes #{ISSUE}\"}}\n"
                )
            })
            .collect();
        std::fs::write(self.ws.join("timeline.txt"), lines).unwrap();
    }
    fn fail_timeline(&self) {
        std::fs::write(self.ws.join("timeline-fail"), "x").unwrap();
    }
    fn set_comments(&self, lines: &str) {
        std::fs::write(self.ws.join("comments.txt"), lines).unwrap();
    }
    fn fail_reads(&self, n: u32) {
        std::fs::write(self.ws.join(format!("read-fail-{n}")), "x").unwrap();
    }
    fn fail_edits(&self, fail: bool) {
        let p = self.ws.join("edit-fail");
        if fail {
            std::fs::write(p, "x").unwrap();
        } else {
            let _ = std::fs::remove_file(p);
        }
    }
    fn calls(&self, prefix: &str) -> Vec<String> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with(prefix))
            .map(str::to_owned)
            .collect()
    }
    /// Every posted comment, whole (a body spans several log lines).
    fn comment_bodies(&self) -> Vec<String> {
        let log = std::fs::read_to_string(&self.log).unwrap_or_default();
        log.split("\nissue comment")
            .skip(usize::from(!log.starts_with("issue comment")))
            .map(str::to_owned)
            .filter(|c| c.contains("loom:noop-hold"))
            .collect()
    }
    fn comments_posted(&self) -> usize {
        self.comment_bodies().len()
    }
}

/// A registry whose forge writes are enabled and a [`Forge`] to drive it. The
/// issue starts open, `loom:important`, `loom:issue`, with no dependencies.
fn forge_registry(threshold: u32) -> (SweepRegistry, Forge, tempfile::TempDir) {
    std::env::remove_var("LOOM_REPO");
    let dir = tempdir().unwrap();
    let ws = dir.path().to_path_buf();
    let log = ws.join("gh.log");
    let script = format!(
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >> \"{log}\"\n\
         if [[ \"$1\" == \"issue\" && \"$2\" == \"edit\" ]]; then\n\
         if [[ -f \"{ws}/edit-fail\" ]]; then printf 'HTTP 422\\n' >&2; exit 1; fi\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$*\" == *timeline* ]]; then\n\
         if [[ -f \"{ws}/timeline-fail\" ]]; then exit 1; fi\n\
         cat \"{ws}/timeline.txt\" 2>/dev/null\n\
         exit 0\n\
         fi\n\
         {gql}\
         if [[ \"$1\" == \"api\" && \"$2\" == \"-X\" && \"$3\" == \"PATCH\" ]]; then\n\
         cp \"$6\" \"{ws}/patched-body.json\"\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$*\" == *issues\\?state=open* ]]; then\n\
         cat \"{ws}/listing.txt\" 2>/dev/null\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$*\" == *comments\\?per_page* ]]; then\n\
         cat \"{ws}/comments-raw.json\" 2>/dev/null || printf '[]'\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$*\" == */comments* ]]; then\n\
         cat \"{ws}/comments.txt\" 2>/dev/null\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/*/issues/* ]]; then\n\
         n=\"${{2##*/}}\"\n\
         if [[ -f \"{ws}/read-fail-$n\" ]]; then exit 1; fi\n\
         if [[ -f \"{ws}/issue-$n.json\" ]]; then cat \"{ws}/issue-$n.json\"; else \
         printf '{{\"state\":\"open\",\"body\":\"\",\"labels\":[]}}\\n'; fi\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
         printf 'rjwalters/loom\\n'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        log = log.display(),
        ws = ws.display(),
        gql = fake_gh_graphql_arm("", 0),
    );
    let fake_gh = ws.join("fake-gh-noop-hold.sh");
    std::fs::write(&fake_gh, script).unwrap();
    let mut perms = std::fs::metadata(&fake_gh).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake_gh, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake_gh) {
        let _ = f.sync_all();
    }
    let mut config = SweepRegistryConfig::new(ws.clone());
    config.gh_bin = Some(fake_gh);
    config.skip_label_flip = false;
    config.journal_path = Some(ws.join("journal.json"));
    let mut reg = SweepRegistry::new(config);
    reg.set_noop_cooldown_config(NoopCooldownConfig {
        hold_threshold: threshold,
        ..NoopCooldownConfig::default()
    });
    let forge = Forge { ws, log };
    forge.set_issue(ISSUE, "open", "", &["loom:issue", "loom:important"]);
    (reg, forge, dir)
}

fn edit_args(forge: &Forge) -> String {
    forge.calls("issue edit").join("\n")
}

/// The loom-ui#694 shape: a priority issue whose remaining work is a human
/// gate. Repeated no-ops must produce at most K dispatches, then the operator
/// park and exactly one comment.
#[test]
#[serial]
fn important_human_gate_issue_is_parked_after_k_dispatches() {
    const K: u32 = 3;
    let (mut reg, forge, _dir) = forge_registry(K);
    let mut dispatches = 0;
    while !reg.noop_hold_applied(ISSUE) && dispatches < 20 {
        dispatches += 1; // one more sweep dispatched and ended as a no-op
        reg.record_noop_release(
            ISSUE,
            Some("remaining work is a human gate; operator must approve".into()),
        );
    }
    assert_eq!(dispatches, K, "at most K dispatches before the hold");
    let edit = edit_args(&forge);
    // #12024: even a human gate parks on loom:blocked, never loom:operator.
    assert!(edit.contains("--add-label loom:blocked"), "{edit}");
    assert_no_operator_write(&forge);
    assert!(edit.contains("--remove-label loom:issue"), "{edit}");
    assert!(!edit.contains("loom:important"), "the priority label is never touched: {edit}");
    assert!(!edit.contains("loom:building"), "the claim is never touched: {edit}");
    assert_eq!(forge.comments_posted(), 1);
    assert!(forge.comment_bodies()[0].contains("What unparks it"));
}

#[test]
#[serial]
fn dependency_blocked_issue_gets_loom_blocked() {
    let (mut reg, forge, _dir) = forge_registry(2);
    forge.set_issue(ISSUE, "open", "## Dependencies\n- [ ] #77 the upstream\n", &["loom:issue"]);
    forge.set_issue(77, "open", "", &[]);
    reg.record_noop_release(ISSUE, None);
    reg.record_noop_release(ISSUE, None);
    assert!(reg.noop_hold_applied(ISSUE));
    let edit = edit_args(&forge);
    assert!(edit.contains("--add-label loom:blocked"), "{edit}");
    assert!(!edit.contains("loom:operator"), "{edit}");
}

#[test]
#[serial]
fn each_changed_input_restarts_the_streak() {
    let (mut reg, forge, _dir) = forge_registry(3);
    reg.record_noop_release(ISSUE, None);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 2);
    // A new human comment.
    forge.set_comments("1:2026-10-04T00:00:00Z\n");
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 1, "a new comment restarts the streak");
    // A label change.
    forge.set_issue(ISSUE, "open", "", &["loom:issue", "loom:important", "x"]);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 1, "a label change restarts the streak");
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 2);
    // Daemon claim/release flapping alone does not.
    forge.set_issue(ISSUE, "open", "", &["loom:building", "loom:important", "x"]);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 3, "claim churn is not a change");
    assert!(forge.calls("issue edit").len() <= 1);
}

#[test]
#[serial]
fn a_dependency_changing_state_restarts_the_streak() {
    let (mut reg, forge, _dir) = forge_registry(3);
    forge.set_issue(ISSUE, "open", "## Dependencies\n- [ ] #77\n", &["loom:issue"]);
    forge.set_issue(77, "open", "", &[]);
    reg.record_noop_release(ISSUE, None);
    reg.record_noop_release(ISSUE, None);
    forge.set_issue(77, "closed", "", &[]);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 1);
}

#[test]
#[serial]
fn failed_label_write_is_not_held_and_not_announced_and_retries_quietly() {
    let (mut reg, forge, _dir) = forge_registry(2);
    forge.fail_edits(true);
    reg.record_noop_release(ISSUE, None);
    reg.record_noop_release(ISSUE, None);
    assert!(!reg.noop_hold_applied(ISSUE), "an unconfirmed park is not a hold");
    assert_eq!(forge.comments_posted(), 1, "one failure notice");
    assert!(forge.comment_bodies()[0].contains("hold-failed"));
    reg.record_noop_release(ISSUE, None);
    assert!(!reg.noop_hold_applied(ISSUE));
    assert_eq!(forge.comments_posted(), 1, "retry must not comment again");
    forge.fail_edits(false);
    reg.record_noop_release(ISSUE, None);
    assert!(reg.noop_hold_applied(ISSUE));
    assert_eq!(forge.comments_posted(), 2, "failure notice + the real hold notice");
    assert!(forge.comment_bodies()[1].contains("kind=hold "));
}

#[test]
#[serial]
fn lifting_the_park_starts_a_fresh_streak_not_an_instant_rehold() {
    let (mut reg, forge, _dir) = forge_registry(2);
    reg.record_noop_release(ISSUE, None);
    reg.record_noop_release(ISSUE, None);
    assert!(reg.noop_hold_applied(ISSUE));
    let edits = forge.calls("issue edit").len();
    // An operator unparks; the issue is dispatched and no-ops again.
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 1);
    assert!(!reg.noop_hold_applied(ISSUE));
    assert_eq!(forge.calls("issue edit").len(), edits, "no immediate re-park");
}

/// Reflect the park the daemon just wrote onto the fake forge, the way the
/// real forge would.
fn reflect_park(forge: &Forge, labels: &[&str]) {
    forge.set_issue(ISSUE, "open", "", labels);
}

#[test]
#[serial]
fn a_changed_input_releases_an_applied_park_without_a_manual_label_edit() {
    let (mut reg, forge, _dir) = forge_registry(2);
    reg.record_noop_release(ISSUE, Some("human gate: operator must approve".into()));
    reg.record_noop_release(ISSUE, Some("human gate: operator must approve".into()));
    assert!(reg.noop_hold_applied(ISSUE));
    let parked = ["loom:important", "loom:blocked"];
    reflect_park(&forge, &parked);
    let t0 = std::time::Instant::now();
    let edits = forge.calls("issue edit").len();

    // Nothing changed (the hold's own notice is excluded): the park stays.
    reg.reconcile_noop_holds(t0 + RECONCILE_INTERVAL);
    assert!(reg.noop_hold_applied(ISSUE));
    assert_eq!(forge.calls("issue edit").len(), edits, "unchanged inputs keep the park");

    // An approval comment arrives while parked.
    forge.set_comments("9:2026-10-08T00:00:00Z\n");
    // Throttled: too soon after the last look.
    reg.reconcile_noop_holds(t0 + RECONCILE_INTERVAL);
    assert!(reg.noop_hold_applied(ISSUE), "reconciliation is rate-limited per issue");

    reg.reconcile_noop_holds(t0 + RECONCILE_INTERVAL * 2);
    assert!(!reg.noop_hold_applied(ISSUE));
    assert_eq!(reg.noop_streak_count(ISSUE), 0, "the streak is dropped");
    let lift = forge.calls("issue edit").pop().unwrap();
    assert!(lift.contains("--remove-label loom:blocked"), "{lift}");
    assert!(lift.contains("--add-label loom:issue"), "{lift}");
    assert_no_operator_write(&forge);
    assert!(!lift.contains("loom:important"), "the priority label is never touched: {lift}");
    assert!(forge
        .comment_bodies()
        .iter()
        .any(|c| c.contains("kind=released")));

    // Once lifted, the next no-op is a fresh streak at 1.
    forge.set_issue(ISSUE, "open", "", &["loom:issue", "loom:important"]);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 1);
}

#[test]
#[serial]
fn a_closed_dependency_releases_a_blocked_park() {
    let (mut reg, forge, _dir) = forge_registry(2);
    let body = "## Dependencies\n- [ ] #77 the upstream\n";
    forge.set_issue(ISSUE, "open", body, &["loom:issue"]);
    forge.set_issue(77, "open", "", &[]);
    reg.record_noop_release(ISSUE, None);
    reg.record_noop_release(ISSUE, None);
    assert!(reg.noop_hold_applied(ISSUE));
    forge.set_issue(ISSUE, "open", body, &["loom:blocked"]);
    forge.set_issue(77, "closed", "", &[]);
    reg.reconcile_noop_holds(std::time::Instant::now() + RECONCILE_INTERVAL * 2);
    assert!(!reg.noop_hold_applied(ISSUE));
    let lift = forge.calls("issue edit").pop().unwrap();
    assert!(lift.contains("--remove-label loom:blocked"), "{lift}");
    assert!(lift.contains("--add-label loom:issue"), "{lift}");
}

/// #11480: lifting the hold's own park never re-arms `loom:issue` past a
/// veto that is not a park (`loom:needs-capability`).
#[test]
#[serial]
fn a_lift_beside_another_veto_does_not_restore_the_queue_label() {
    let (mut reg, forge, _dir) = forge_registry(2);
    let body = "## Dependencies\n- [ ] #77 the upstream\n";
    forge.set_issue(ISSUE, "open", body, &["loom:issue"]);
    forge.set_issue(77, "open", "", &[]);
    reg.record_noop_release(ISSUE, None);
    reg.record_noop_release(ISSUE, None);
    assert!(reg.noop_hold_applied(ISSUE));
    forge.set_issue(ISSUE, "open", body, &["loom:blocked", "loom:needs-capability"]);
    forge.set_issue(77, "closed", "", &[]);
    reg.reconcile_noop_holds(std::time::Instant::now() + RECONCILE_INTERVAL * 2);
    assert!(!reg.noop_hold_applied(ISSUE));
    let lift = forge.calls("issue edit").pop().unwrap();
    assert!(lift.contains("--remove-label loom:blocked"), "{lift}");
    assert!(!lift.contains("--add-label loom:issue"), "{lift}");
}

#[test]
#[serial]
fn an_operator_who_already_lifted_the_park_is_not_edited_again() {
    let (mut reg, forge, _dir) = forge_registry(2);
    reg.record_noop_release(ISSUE, Some("human gate".into()));
    reg.record_noop_release(ISSUE, Some("human gate".into()));
    assert!(reg.noop_hold_applied(ISSUE));
    let edits = forge.calls("issue edit").len();
    forge.set_issue(ISSUE, "open", "", &["loom:issue", "x"]);
    reg.reconcile_noop_holds(std::time::Instant::now() + RECONCILE_INTERVAL * 2);
    assert!(!reg.noop_hold_applied(ISSUE));
    assert_eq!(forge.calls("issue edit").len(), edits, "nothing left to undo");
}

/// `COMMENTS_JQ` is evaluated by `gh`, which the fake bypasses, so run the
/// real filter through `jq`: only a bot or the hold's own notice drops out.
#[test]
fn comment_filter_keeps_a_human_comment_that_carries_another_loom_marker() {
    use std::io::Write;
    let comments = serde_json::json!([
        {"id": 1, "updated_at": "t1", "user": {"type": "User"}, "body": "plain"},
        {"id": 2, "updated_at": "t2", "user": {"type": "User"},
         "body": "approved <!-- loom:verdict-sha sha=abc -->"},
        {"id": 3, "updated_at": "t3", "user": {"type": "Bot"}, "body": "bot"},
        {"id": 4, "updated_at": "t4", "user": {"type": "User"},
         "body": format!("{NOOP_HOLD_COMMENT_MARKER}\nheld")},
    ]);
    let mut child = std::process::Command::new("jq")
        .args(["-r", COMMENTS_JQ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("jq");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(comments.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1:t1\n2:t2\n");
}

#[test]
#[serial]
fn zero_threshold_disables_the_hold_without_touching_the_cooldown() {
    let (mut reg, forge, _dir) = forge_registry(0);
    for _ in 0..5 {
        reg.record_noop_release(ISSUE, None);
    }
    assert_eq!(reg.noop_streak_count(ISSUE), 0);
    assert!(forge.calls("issue edit").is_empty());
    assert!(forge.calls("api").is_empty(), "no forge reads when disabled");
    assert!(reg.noop_cooldown_remaining(ISSUE, Utc::now()).is_some());
    assert_eq!(reg.noop_release_count(ISSUE), 5);
}

#[test]
#[serial]
fn unreadable_forge_never_counts_or_parks() {
    let (mut reg, forge, _dir) = forge_registry(1);
    std::fs::write(forge.ws.join("issue-694.json"), "not json").unwrap();
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 0);
    assert!(forge.calls("issue edit").is_empty());
}

#[test]
#[serial]
fn already_parked_or_closed_issues_keep_no_streak() {
    let (mut reg, forge, _dir) = forge_registry(2);
    forge.set_issue(ISSUE, "open", "", &["loom:blocked"]);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 0);
    forge.set_issue(ISSUE, "closed", "", &["loom:issue"]);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 0);
}

/// Issue #11438: the under-60-s form of the same loop — a checkpoint-less
/// clean exit that reached no phase at all and never self-reported — feeds
/// the hold from the reaper's own observations, once per dispatch.
#[test]
#[serial]
fn clean_phaseless_reaper_outcomes_feed_the_hold() {
    let (mut reg, forge, _dir) = forge_registry(3);
    // A non-clean exit is the crash path, not this mechanism's shape.
    reg.note_clean_phaseless_exit_outcome(ISSUE, "sweep-1", Some(OpenPrProbe::NoneOpen), Some(1));
    // An unverified PR probe never counts (the #4452 fail-open contract).
    reg.note_clean_phaseless_exit_outcome(ISSUE, "sweep-1", None, Some(0));
    reg.note_clean_phaseless_exit_outcome(
        ISSUE,
        "sweep-1",
        Some(OpenPrProbe::ProbeFailed),
        Some(0),
    );
    assert_eq!(reg.noop_streak_count(ISSUE), 0);
    // The verified shape counts once per dispatch.
    reg.note_clean_phaseless_exit_outcome(ISSUE, "sweep-1", Some(OpenPrProbe::NoneOpen), Some(0));
    reg.note_clean_phaseless_exit_outcome(ISSUE, "sweep-1", Some(OpenPrProbe::NoneOpen), Some(0));
    assert_eq!(reg.noop_streak_count(ISSUE), 1, "one dispatch counts once");
    reg.note_clean_phaseless_exit_outcome(ISSUE, "sweep-2", Some(OpenPrProbe::NoneOpen), Some(0));
    assert!(reg.noop_loop_kind(ISSUE, "sweep-3").is_some(), "the next outcome is a loop");
    reg.note_clean_phaseless_exit_outcome(ISSUE, "sweep-3", Some(OpenPrProbe::NoneOpen), Some(0));
    assert!(reg.noop_hold_applied(ISSUE));
    assert!(edit_args(&forge).contains("--add-label loom:blocked"));
    assert_no_operator_write(&forge);
}

/// A sweep that DID self-report was already counted at IPC time
/// (`record_noop_release` → `note_noop_for_hold`); the reaper's feed must not
/// count the same dispatch a second time (#11438).
#[test]
#[serial]
fn a_self_reported_clean_noop_is_not_counted_twice() {
    let (mut reg, _forge, _dir) = forge_registry(3);
    let sweep_id =
        crate::sweep_registry::test_support::insert_clean_exit_running(&mut reg, ISSUE, 0);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 1);
    reg.note_clean_phaseless_exit_outcome(ISSUE, &sweep_id, Some(OpenPrProbe::NoneOpen), Some(0));
    assert_eq!(
        reg.noop_streak_count(ISSUE),
        1,
        "the reaper's feed must not double-count a self-reported dispatch"
    );
}

/// A sweep that never calls `noop-cooldown record` is still counted from the
/// reaper's Curator-only classification, once per dispatch.
#[test]
#[serial]
fn curator_only_reaper_outcomes_are_counted_without_the_ipc_call() {
    let (mut reg, forge, _dir) = forge_registry(3);
    // A sweep that got past the Curator is not a no-op loop.
    reg.note_curator_only_outcome(ISSUE, "sweep-a", Some("builder-done"));
    assert_eq!(reg.noop_streak_count(ISSUE), 0);
    reg.note_curator_only_outcome(ISSUE, "sweep-1", Some("curator-done"));
    reg.note_curator_only_outcome(ISSUE, "sweep-1", Some("curator-done"));
    assert_eq!(reg.noop_streak_count(ISSUE), 1, "one dispatch counts once");
    reg.note_curator_only_outcome(ISSUE, "sweep-2", Some("curator-done"));
    assert!(reg.noop_loop_kind(ISSUE, "sweep-3").is_some(), "the next outcome is a loop");
    reg.note_curator_only_outcome(ISSUE, "sweep-3", Some("curator-done"));
    assert!(reg.noop_hold_applied(ISSUE));
    assert!(edit_args(&forge).contains("--add-label loom:blocked"));
    assert_no_operator_write(&forge);
}

#[test]
#[serial]
fn loop_kind_is_absent_for_a_first_time_stop() {
    let (mut reg, _forge, _dir) = forge_registry(3);
    assert_eq!(reg.noop_loop_kind(ISSUE, "sweep-1"), None);
    reg.note_curator_only_outcome(ISSUE, "sweep-1", Some("curator-done"));
    assert_eq!(reg.noop_loop_kind(ISSUE, "sweep-1"), None, "streak of 1 is not a loop");
    assert_eq!(
        reg.noop_loop_kind(ISSUE, "sweep-2"),
        Some(ParkKind::HumanGate),
        "a second consecutive stop is"
    );
}

#[test]
fn parks_are_already_skipped_by_every_candidate_path() {
    // The park is only durable because the work finder applies SKIP_LABELS to
    // every candidate regardless of its priority label.
    for kind in [ParkKind::Blocked, ParkKind::HumanGate] {
        assert!(kind
            .labels()
            .iter()
            .any(|l| crate::work_finder::SKIP_LABELS.contains(l)));
    }
}

#[test]
#[serial]
fn threshold_resolves_env_over_config_over_default() {
    let dir = tempdir().unwrap();
    std::env::remove_var(NOOP_HOLD_THRESHOLD_ENV);
    assert_eq!(resolve_noop_cooldown_config(dir.path()).hold_threshold, 3);
    std::env::set_var(NOOP_HOLD_THRESHOLD_ENV, "5");
    assert_eq!(resolve_noop_cooldown_config(dir.path()).hold_threshold, 5);
    std::env::set_var(NOOP_HOLD_THRESHOLD_ENV, "0");
    assert_eq!(resolve_noop_cooldown_config(dir.path()).hold_threshold, 0);
    std::env::remove_var(NOOP_HOLD_THRESHOLD_ENV);
}

#[test]
#[serial]
fn an_unreadable_dependency_never_releases_an_applied_hold() {
    let (mut reg, forge, _dir) = forge_registry(2);
    let body = "## Dependencies\n- [ ] #77 the upstream\n";
    forge.set_issue(ISSUE, "open", body, &["loom:issue"]);
    forge.set_issue(77, "open", "", &[]);
    reg.record_noop_release(ISSUE, None);
    reg.record_noop_release(ISSUE, None);
    assert!(reg.noop_hold_applied(ISSUE));
    forge.set_issue(ISSUE, "open", body, &["loom:blocked"]);
    forge.fail_reads(77);
    let edits = forge.calls("issue edit").len();
    reg.reconcile_noop_holds(std::time::Instant::now() + RECONCILE_INTERVAL * 2);
    assert!(reg.noop_hold_applied(ISSUE), "a transient read failure keeps the hold");
    assert_eq!(forge.calls("issue edit").len(), edits, "no label write on an unread dependency");
}

/// Every named dependency is fingerprinted, not a prefix: closing the seventh
/// of eight releases the hold.
#[test]
#[serial]
fn a_dependency_beyond_the_sixth_releases_an_applied_hold() {
    let (mut reg, forge, _dir) = forge_registry(2);
    let body: String = std::iter::once("## Dependencies\n".to_string())
        .chain((71..=78).map(|n| format!("- [ ] #{n} upstream\n")))
        .collect();
    forge.set_issue(ISSUE, "open", &body, &["loom:issue"]);
    for n in 71..=78 {
        forge.set_issue(n, "open", "", &[]);
    }
    reg.record_noop_release(ISSUE, None);
    reg.record_noop_release(ISSUE, None);
    assert!(reg.noop_hold_applied(ISSUE));
    forge.set_issue(ISSUE, "open", &body, &["loom:blocked"]);
    let t0 = std::time::Instant::now();
    reg.reconcile_noop_holds(t0);
    assert!(reg.noop_hold_applied(ISSUE), "unchanged dependencies keep the hold");
    forge.set_issue(78, "closed", "", &[]);
    reg.reconcile_noop_holds(t0 + RECONCILE_INTERVAL * 2);
    assert!(!reg.noop_hold_applied(ISSUE), "an omitted-prefix dependency closing releases");
}

/// A registry over `forge`'s existing workspace: a restarted daemon, or
/// another host sharing the same forge.
fn registry_over(dir: &Path, threshold: u32) -> SweepRegistry {
    let mut config = SweepRegistryConfig::new(dir.to_path_buf());
    config.gh_bin = Some(dir.join("fake-gh-noop-hold.sh"));
    config.skip_label_flip = false;
    config.journal_path = Some(dir.join("journal.json"));
    let mut reg = SweepRegistry::new(config);
    reg.set_noop_cooldown_config(NoopCooldownConfig {
        hold_threshold: threshold,
        ..NoopCooldownConfig::default()
    });
    reg
}

#[test]
#[serial]
fn an_applied_hold_is_released_after_a_daemon_restart() {
    let (mut reg, forge, dir) = forge_registry(2);
    reg.record_noop_release(ISSUE, Some("human gate: operator must approve".into()));
    reg.record_noop_release(ISSUE, Some("human gate: operator must approve".into()));
    assert!(reg.noop_hold_applied(ISSUE));
    reflect_park(&forge, &["loom:important", "loom:blocked"]);

    // A fresh registry over the same logs dir: the in-memory table is empty.
    let mut restarted = registry_over(dir.path(), 2);
    assert!(!restarted.noop_hold_applied(ISSUE));

    // Unchanged inputs: the recovered hold stays.
    let t0 = std::time::Instant::now();
    restarted.reconcile_noop_holds(t0);
    assert!(restarted.noop_hold_applied(ISSUE), "hold recovered from disk");

    // An approval comment lands; the recovered hold is released unaided.
    forge.set_comments("9:2026-10-08T00:00:00Z\n");
    restarted.reconcile_noop_holds(t0 + RECONCILE_INTERVAL * 2);
    assert!(!restarted.noop_hold_applied(ISSUE));
    let lift = forge.calls("issue edit").pop().unwrap();
    assert!(lift.contains("--remove-label loom:blocked"), "{lift}");
    assert!(lift.contains("--add-label loom:issue"), "{lift}");
    assert_no_operator_write(&forge);
}

/// No `issue edit` the daemon made adds or removes an operator hold, under
/// any of its names (#12024, the #11861 rule).
fn assert_no_operator_write(forge: &Forge) {
    let edits = edit_args(forge);
    assert!(
        !edits.contains("loom:operator"),
        "the no-op hold touched loom:operator: {edits}"
    );
}

// --- #12024: the hold parks on loom:blocked, counted across the fleet -------

/// One simulated host: a registry over the shared fake forge, with its own
/// peer view and its own outbound ad queue.
struct Host {
    reg: SweepRegistry,
    name: String,
    view: Arc<Mutex<PeerClaimView>>,
    rx: tokio::sync::mpsc::Receiver<ClaimAd>,
}

fn host(dir: &Path, name: &str, threshold: u32) -> Host {
    let mut reg = registry_over(dir, threshold);
    let view = Arc::new(Mutex::new(PeerClaimView::new(name.into(), Duration::from_secs(120))));
    reg.set_peer_claims(Arc::clone(&view));
    let (tx, rx) = tokio::sync::mpsc::channel(64);
    reg.set_peer_claim_publisher(tx);
    Host {
        reg,
        name: name.into(),
        view,
        rx,
    }
}

/// Deliver every ad each host published to every other host, stamped with
/// the sender's name (one process has one `host_identity()`).
fn relay(hosts: &mut [Host]) {
    let mut sent = Vec::new();
    for h in hosts.iter_mut() {
        while let Ok(mut ad) = h.rx.try_recv() {
            ad.host.clone_from(&h.name);
            sent.push(ad);
        }
    }
    for ad in &sent {
        for h in hosts.iter() {
            crate::peer_claims::observe_brake_ad(
                &mut h.view.lock().unwrap(),
                ad,
                std::time::Instant::now(),
            );
        }
    }
}

/// The #12024 acceptance case: three no-ops on three different hosts park
/// the issue at the threshold, although no host saw more than one.
#[test]
#[serial]
fn no_ops_across_hosts_park_the_item_at_the_threshold() {
    let (_reg, forge, dir) = forge_registry(3);
    let reason = Some("remaining work is an operator decision".to_string());
    let mut hosts: Vec<Host> = ["host-a", "host-b", "host-c"]
        .iter()
        .map(|n| host(dir.path(), n, 3))
        .collect();
    for i in 0..2 {
        hosts[i].reg.record_noop_release(ISSUE, reason.clone());
        relay(&mut hosts);
        assert!(!hosts[i].reg.noop_hold_applied(ISSUE), "host {i} saw only {}", i + 1);
    }
    assert!(forge.calls("issue edit").is_empty(), "nothing parked below the threshold");
    hosts[2].reg.record_noop_release(ISSUE, reason.clone());
    assert!(hosts[2].reg.noop_hold_applied(ISSUE), "1 here + 2 on peers = 3");
    assert_eq!(hosts[2].reg.noop_streak_count(ISSUE), 1, "this host's own count is 1");
    let edit = edit_args(&forge);
    assert!(edit.contains("--add-label loom:blocked"), "{edit}");
    assert_no_operator_write(&forge);
    // The park record went into the body before the label.
    let patched = std::fs::read_to_string(forge.ws.join("patched-body.json")).unwrap();
    assert!(patched.contains("reason=\\\"no-op hold\\\""), "{patched}");
    assert!(patched.contains("by=daemon"), "{patched}");
    // The notice records the digest any host can adopt it from.
    assert!(forge.comment_bodies()[0].contains(NOOP_HOLD_FP_MARKER_PREFIX));

    // The park broke the fleet streak: a human lifts the park, and host A's
    // stale count does not re-park on the next no-op anywhere.
    relay(&mut hosts);
    reflect_park(&forge, &["loom:issue", "loom:important"]);
    hosts[0].reg.record_noop_release(ISSUE, reason.clone());
    relay(&mut hosts);
    assert_eq!(hosts[0].reg.noop_streak_count(ISSUE), 1, "the break reset host A");
    assert!(!hosts[0].reg.noop_hold_applied(ISSUE));
    assert_eq!(forge.calls("issue edit").len(), 1, "no immediate re-park");
}

/// A peer count taken against another state of the issue is not part of the
/// streak: a fingerprint change restarts the fleet streak too.
#[test]
#[serial]
fn a_peer_count_on_another_fingerprint_does_not_add_up() {
    let (_reg, forge, dir) = forge_registry(3);
    let mut hosts: Vec<Host> = ["host-a", "host-b"]
        .iter()
        .map(|n| host(dir.path(), n, 3))
        .collect();
    hosts[0].reg.record_noop_release(ISSUE, None);
    hosts[0].reg.record_noop_release(ISSUE, None);
    relay(&mut hosts);
    // A human comment lands: host A's two no-ops were on the old state.
    forge.set_comments("5:2026-10-14T00:00:00Z\n");
    hosts[1].reg.record_noop_release(ISSUE, None);
    assert!(!hosts[1].reg.noop_hold_applied(ISSUE), "1, not 1 + 2");
    hosts[1].reg.record_noop_release(ISSUE, None);
    hosts[1].reg.record_noop_release(ISSUE, None);
    assert!(hosts[1].reg.noop_hold_applied(ISSUE), "three on the new state park");
}

/// A parked item is not offered again until its fingerprint changes: the
/// park is a skip label, an unchanged reconciliation writes nothing, and
/// only a change lifts it (and restores `loom:issue`).
#[test]
#[serial]
fn a_parked_item_is_not_repromoted_until_its_fingerprint_changes() {
    let (mut reg, forge, _dir) = forge_registry(2);
    forge.set_issue(ISSUE, "open", "", &["loom:curated", "loom:important"]);
    reg.record_noop_release(ISSUE, None);
    reg.record_noop_release(ISSUE, None);
    assert!(
        reg.noop_hold_applied(ISSUE),
        "a curated-only item is parked, not left promotable"
    );
    let parked = ["loom:curated", "loom:important", "loom:blocked"];
    assert!(crate::work_finder::SKIP_LABELS.contains(&"loom:blocked"));
    assert!(crate::work_finder::is_blocked_hold(&parked));
    reflect_park(&forge, &parked);
    let edits = forge.calls("issue edit").len();
    let t0 = std::time::Instant::now();
    for k in 1..=4 {
        reg.reconcile_noop_holds(t0 + RECONCILE_INTERVAL * k);
        assert!(reg.noop_hold_applied(ISSUE));
    }
    assert_eq!(forge.calls("issue edit").len(), edits, "unchanged: nothing re-offers it");
    // The fingerprint changes (a linked PR appears): the hold releases.
    forge.set_linked_prs(&[(12, "open", false)]);
    reg.reconcile_noop_holds(t0 + RECONCILE_INTERVAL * 6);
    assert!(!reg.noop_hold_applied(ISSUE));
    let lift = forge.calls("issue edit").pop().unwrap();
    assert!(lift.contains("--remove-label loom:blocked"), "{lift}");
    assert!(lift.contains("--add-label loom:issue"), "{lift}");
    assert_no_operator_write(&forge);
}

/// A hold an older daemon applied as `loom:operator`, recovered from its
/// holds file, is never edited by the daemon: only a human (or `labels
/// migrate --holds`) removes that label (#11861, #12024).
#[test]
#[serial]
fn a_legacy_operator_hold_is_never_lifted_by_the_daemon() {
    let (_reg, forge, dir) = forge_registry(2);
    let full = "labels=[loom:important];open=true;comments=[];pr=none;deps=[]";
    std::fs::create_dir_all(dir.path().join(".loom/logs")).unwrap();
    let file =
        serde_json::json!({ ISSUE.to_string(): {"park": "human-gate", "fingerprint": full} });
    let mut reg = registry_over(dir.path(), 2);
    std::fs::create_dir_all(reg.config().logs_dir()).unwrap();
    std::fs::write(reg.config().logs_dir().join("noop-holds.json"), file.to_string()).unwrap();
    reflect_park(&forge, &["loom:important", "loom:operator"]);
    forge.set_comments("9:2026-10-08T00:00:00Z\n");
    reg.reconcile_noop_holds(std::time::Instant::now());
    assert!(!reg.noop_hold_applied(ISSUE), "the daemon lets go of it");
    assert!(forge.calls("issue edit").is_empty(), "and writes nothing");
}

/// The migrated shape (#12024): `loom:blocked` + the no-op park record + a
/// pre-#12024 notice. A host that never applied it adopts it from the forge
/// and releases it on a fingerprint change.
#[test]
#[serial]
fn a_migrated_hold_is_adopted_from_the_forge_and_released_on_change() {
    let (_reg, forge, dir) = forge_registry(2);
    let record = crate::sweep_registry::park_hold::render_hold_record(
        crate::sweep_registry::park_hold::NOOP_HOLD_REASON,
        "2026-10-14T00:00:00Z",
    );
    let body = format!("The work.\n\n{record}\n");
    forge.set_issue(ISSUE, "open", &body, &["loom:curated", "loom:blocked"]);
    // The fingerprint the old notice recorded: pre-park labels.
    let full = "labels=[loom:curated];open=true;comments=[];pr=none;deps=[]";
    let notice = format!(
        "{NOOP_HOLD_COMMENT_MARKER}\n{NOOP_HOLD_APPLIED_COMMENT_MARKER}\nFingerprint compared: `{full}`"
    );
    std::fs::write(
        forge.ws.join("comments-raw.json"),
        serde_json::json!([{"body": notice, "author_association": "MEMBER",
                            "user": {"login": "someone", "type": "User"}}])
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        forge.ws.join("listing.txt"),
        format!("{}\n", serde_json::json!({"number": ISSUE, "body": body})),
    )
    .unwrap();
    let mut reg = registry_over(dir.path(), 2);
    assert_eq!(reg.adopt_forge_noop_holds(), 1);
    assert!(reg.noop_hold_applied(ISSUE));
    assert_eq!(reg.adopt_forge_noop_holds(), 0, "once per process");
    let t0 = std::time::Instant::now();
    reg.reconcile_noop_holds(t0);
    assert!(reg.noop_hold_applied(ISSUE), "unchanged since the old notice");
    assert!(forge.calls("issue edit").is_empty());
    forge.set_comments("9:2026-10-15T00:00:00Z\n");
    reg.reconcile_noop_holds(t0 + adopt::ADOPTED_RECONCILE_INTERVAL);
    assert!(!reg.noop_hold_applied(ISSUE), "a new comment releases it");
    let lift = forge.calls("issue edit").pop().unwrap();
    assert!(lift.contains("--remove-label loom:blocked"), "{lift}");
    assert_no_operator_write(&forge);
}

/// The checkpointed crash wrapper (what `reap_once` calls) feeds the hold for a
/// Curator-only death, and skips an externally-killed one.
#[test]
#[serial]
fn checkpointed_curator_only_crash_feeds_the_hold_unless_externally_killed() {
    let (mut reg, _forge, _dir) = forge_registry(3);
    reg.note_prless_crash_outcome(ISSUE, "sweep-oom", Some(137), 60, Some("curator-done"));
    assert_eq!(reg.noop_streak_count(ISSUE), 0, "an OOM kill is environmental");
    reg.note_prless_crash_outcome(ISSUE, "sweep-b", Some(1), 60, Some("builder-done"));
    assert_eq!(reg.noop_streak_count(ISSUE), 0, "past the Curator is not a no-op");
    reg.note_prless_crash_outcome(ISSUE, "sweep-1", Some(1), 60, Some("curator-done"));
    assert_eq!(reg.noop_streak_count(ISSUE), 1);
}

/// A linked PR going closed-unmerged -> merged, or a second linked PR changing
/// while the first stays open, each restart the streak (the "linked PR state
/// changed" unpark criterion).
#[test]
#[serial]
fn linked_pr_set_and_state_changes_restart_the_streak() {
    let (mut reg, forge, _dir) = forge_registry(5);
    forge.set_linked_prs(&[(10, "closed", false)]);
    reg.record_noop_release(ISSUE, None);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 2);
    forge.set_linked_prs(&[(10, "closed", true)]);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 1, "closed -> merged is a change");
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 2);
    forge.set_linked_prs(&[(10, "open", false), (11, "open", false)]);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 1, "a second linked PR is a change");
    reg.record_noop_release(ISSUE, None);
    forge.set_linked_prs(&[(10, "open", false), (11, "closed", false)]);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 1, "the second PR changing is a change");
}

/// A bare mention is not a link: it must not move the fingerprint.
#[test]
#[serial]
fn a_pr_that_only_mentions_the_issue_is_not_a_linked_pr() {
    let (mut reg, forge, _dir) = forge_registry(5);
    reg.record_noop_release(ISSUE, None);
    std::fs::write(
        forge.ws.join("timeline.txt"),
        format!("{{\"number\":9,\"state\":\"open\",\"merged\":false,\"body\":\"see #{ISSUE}\"}}\n"),
    )
    .unwrap();
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 2);
}

/// An unreadable timeline is inconclusive: never counted, never a release.
#[test]
#[serial]
fn an_unreadable_linked_pr_timeline_is_inconclusive() {
    let (mut reg, forge, _dir) = forge_registry(3);
    reg.record_noop_release(ISSUE, None);
    forge.fail_timeline();
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 1);
}

/// The checkpoint-less exit path honors the same external-kill exemption as
/// the checkpointed one: exit 137/143 never advances the hold streak.
#[test]
#[serial]
fn checkpointless_curator_only_exit_skips_external_kills() {
    let (mut reg, _forge, _dir) = forge_registry(3);
    for code in [137, 143] {
        let sweep = format!("sweep-k{code}");
        reg.seed_curator_only_history_for_test(&sweep);
        reg.note_prless_exit_outcome(ISSUE, &sweep, None, Some(code), 60);
        assert_eq!(reg.noop_streak_count(ISSUE), 0, "exit {code} is environmental");
    }
    reg.seed_curator_only_history_for_test("sweep-c");
    reg.note_prless_exit_outcome(ISSUE, "sweep-c", None, Some(1), 60);
    assert_eq!(reg.noop_streak_count(ISSUE), 1);
}
