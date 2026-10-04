//! Tests for the no-op hold (Issue #10156), driven through a fake `gh` whose
//! forge state is a handful of files the test can edit between no-ops.

use super::*;
use crate::sweep_registry::test_support::{fake_gh_graphql_arm, fake_gh_timeline_rest_arm};
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
    fn set_comments(&self, lines: &str) {
        std::fs::write(self.ws.join("comments.txt"), lines).unwrap();
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
         {timeline}{gql}\
         if [[ \"$1\" == \"api\" && \"$*\" == */comments* ]]; then\n\
         cat \"{ws}/comments.txt\" 2>/dev/null\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/*/issues/* ]]; then\n\
         n=\"${{2##*/}}\"\n\
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
        timeline = fake_gh_timeline_rest_arm("", 0),
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
