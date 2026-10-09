//! Tests for the no-op hold (Issue #10156), driven through a fake `gh` whose
//! forge state is a handful of files the test can edit between no-ops.

use super::*;
use crate::sweep_registry::test_support::fake_gh_graphql_arm;
use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
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
/// issue starts open, starred, `loom:issue`, with no dependencies.
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
    forge.set_issue(ISSUE, "open", "", &["loom:issue", "loom:operator-priority"]);
    (reg, forge, dir)
}

fn edit_args(forge: &Forge) -> String {
    forge.calls("issue edit").join("\n")
}

/// The loom-ui#694 shape: a starred issue whose remaining work is a human
/// gate. Repeated no-ops must produce at most K dispatches, then the operator
/// park and exactly one comment.
#[test]
#[serial]
fn starred_human_gate_issue_is_parked_after_k_dispatches() {
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
    assert!(edit.contains("--add-label loom:operator-only"), "{edit}");
    assert!(edit.contains("--add-label loom:operator-decision"), "{edit}");
    assert!(edit.contains("--remove-label loom:issue"), "{edit}");
    assert!(!edit.contains("loom:operator-priority"), "the star is never touched: {edit}");
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
    assert!(!edit.contains("loom:operator-only"), "{edit}");
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
    forge.set_issue(ISSUE, "open", "", &["loom:issue", "loom:operator-priority", "x"]);
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 1, "a label change restarts the streak");
    reg.record_noop_release(ISSUE, None);
    assert_eq!(reg.noop_streak_count(ISSUE), 2);
    // Daemon claim/release flapping alone does not.
    forge.set_issue(ISSUE, "open", "", &["loom:building", "loom:operator-priority", "x"]);
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
    let parked = [
        "loom:operator-priority",
        "loom:operator-only",
        "loom:operator-decision",
    ];
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
    assert!(lift.contains("--remove-label loom:operator-only"), "{lift}");
    assert!(lift.contains("--remove-label loom:operator-decision"), "{lift}");
    assert!(lift.contains("--add-label loom:issue"), "{lift}");
    assert!(!lift.contains("loom:operator-priority"), "the star is never touched: {lift}");
    assert!(forge
        .comment_bodies()
        .iter()
        .any(|c| c.contains("kind=released")));

    // Once lifted, the next no-op is a fresh streak at 1.
    forge.set_issue(ISSUE, "open", "", &["loom:issue", "loom:operator-priority"]);
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
    assert!(edit_args(&forge).contains("loom:operator-only"));
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
    // every candidate regardless of `loom:operator-priority`.
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

#[test]
#[serial]
fn an_applied_hold_is_released_after_a_daemon_restart() {
    let (mut reg, forge, dir) = forge_registry(2);
    reg.record_noop_release(ISSUE, Some("human gate: operator must approve".into()));
    reg.record_noop_release(ISSUE, Some("human gate: operator must approve".into()));
    assert!(reg.noop_hold_applied(ISSUE));
    let parked = [
        "loom:operator-priority",
        "loom:operator-only",
        "loom:operator-decision",
    ];
    reflect_park(&forge, &parked);

    // A fresh registry over the same logs dir: the in-memory table is empty.
    let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
    config.gh_bin = Some(dir.path().join("fake-gh-noop-hold.sh"));
    config.skip_label_flip = false;
    config.journal_path = Some(dir.path().join("journal.json"));
    let mut restarted = SweepRegistry::new(config);
    restarted.set_noop_cooldown_config(NoopCooldownConfig {
        hold_threshold: 2,
        ..NoopCooldownConfig::default()
    });
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
    assert!(lift.contains("--remove-label loom:operator-only"), "{lift}");
    assert!(lift.contains("--add-label loom:issue"), "{lift}");
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
