//! Tests for the exhausted-budget base-sync handoff (#10388), against a stub
//! `gh` (injected as a function argument, see `redate::gh_api_with`).

use super::*;
use crate::merge_pr::redate::tests::{now, split_stub_calls, tmp_dir};
use crate::merge_pr::redate::{
    budget::attempt_marker, hold_marker, redate_marker, remedy_with_sync, Attribution,
    BudgetConfig, HOLD_LABEL,
};
use serde_json::json;
use std::fs;

const FLEET: &str = "loom-fleet-dispatch[bot]";
const CFG3: BudgetConfig = BudgetConfig {
    budget: 3,
    backoff_secs: 600,
};

fn comment(login: &str, body: &str) -> Value {
    json!({
        "user": {"login": login, "type": if login.ends_with("[bot]") { "Bot" } else { "User" }},
        "author_association": "NONE",
        "body": body,
        "created_at": "2026-09-30T08:00:00Z",
    })
}

fn exhausted_chain() -> Value {
    comment(
        FLEET,
        &format!("{}\n{}", redate_marker("abc0000"), attempt_marker("abc0000", 3)),
    )
}

/// `mode`: `ok` (202), `conflict` (422 merge conflict), `other` (some other failure).
fn stub(name: &str, listing: &Value, mode: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = tmp_dir(name);
    fs::write(dir.join("comments.json"), listing.to_string()).expect("listing");
    let script = format!(
        r#"#!/usr/bin/env bash
shift
BODY=""; prev=""; PATH_ARG=""
for arg in "$@"; do
  if [ "$prev" = "--input" ]; then
    if [ "$arg" = "-" ]; then BODY="$(cat)"; else BODY="$(cat -- "$arg")"; fi
  fi
  case "$arg" in repos/*) PATH_ARG="$arg" ;; esac
  prev="$arg"
done
{{ printf '%s\nSTDIN:%s\n<<<REDATE-STUB-CALL-END>>>\n' "$*" "$BODY"; }} >> "{dir}/argv.log"
case "$PATH_ARG" in
  */git/refs/heads/*) echo abc0000 ;;
  */pulls/*/update-branch)
    case "{mode}" in
      ok) echo '{{"message":"Updating pull request branch."}}' ;;
      conflict) echo "gh: merge conflict between base and head (HTTP 422)" >&2; exit 1 ;;
      *) echo "gh: Server Error (HTTP 500)" >&2; exit 1 ;;
    esac ;;
  */issues/*/comments)
    if [ -n "$BODY" ]; then echo '{{"id":1}}'; else cat "{dir}/comments.json"; fi ;;
  */issues/*/labels) echo '[]' ;;
  *) echo "stub: unexpected $PATH_ARG" >&2; exit 2 ;;
esac
"#,
        dir = dir.display()
    );
    let gh = dir.join("gh");
    fs::write(&gh, script).expect("script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    (dir, gh)
}

fn go(gh: &std::path::Path, handoffs: u32) -> RemedyOutcome {
    remedy_with_sync(
        gh.to_str().unwrap(),
        "o/r",
        "feature/x",
        "abc0000",
        "42",
        CFG3,
        SyncConfig { handoffs },
        now(),
        || None::<Attribution>,
    )
}

/// Whether the log shows `label` being applied via the labels API payload.
fn applied(log: &str, label: &str) -> bool {
    log.contains(&format!("{{\"labels\":[\"{label}\"]}}"))
}

fn argv(dir: &std::path::Path) -> String {
    fs::read_to_string(dir.join("argv.log")).expect("argv log")
}

#[test]
fn exhausted_with_no_markers_syncs_records_and_does_not_hold() {
    let (dir, gh) = stub("sync-ok", &json!([exhausted_chain()]), "ok");
    assert_eq!(
        go(&gh, 1),
        RemedyOutcome::SyncedBase {
            head: "abc0000".into(),
            n: 1,
            max: 1
        }
    );
    let log = argv(&dir);
    assert!(log.contains("-X PUT repos/o/r/pulls/42/update-branch"), "{log}");
    assert!(log.contains("expected_head_sha=abc0000"), "{log}");
    assert!(log.contains(&sync_marker("abc0000", 1)), "{log}");
    assert!(log.contains("1 of 1") && log.contains("3 of 3"), "cycle counts: {log}");
    assert!(!applied(&log, HOLD_LABEL) && !log.contains(&hold_marker("abc0000")), "{log}");
    assert!(!log.contains("/labels"), "no label at all on a clean sync: {log}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_conflict_applies_merge_conflict_and_not_the_operator_hold() {
    let (dir, gh) = stub("sync-conflict", &json!([exhausted_chain()]), "conflict");
    assert_eq!(
        go(&gh, 1),
        RemedyOutcome::SyncConflict {
            notice_posted: true
        }
    );
    let log = argv(&dir);
    assert!(log.contains(&conflict_marker("abc0000")), "{log}");
    assert!(!applied(&log, HOLD_LABEL) && !applied(&log, "loom:changes-requested"), "{log}");
    assert!(applied(&log, CONFLICT_LABEL), "{log}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_repeat_conflict_does_not_repost_the_notice() {
    let listing = json!([
        exhausted_chain(),
        comment(FLEET, &conflict_marker("abc0000"))
    ]);
    let (dir, gh) = stub("sync-conflict-again", &listing, "conflict");
    assert_eq!(
        go(&gh, 1),
        RemedyOutcome::SyncConflict {
            notice_posted: false
        }
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_spent_handoff_escalates_exactly_as_before() {
    // The trusted sync marker is for an EARLIER head: the count is per PR.
    let listing = json!([
        exhausted_chain(),
        comment(FLEET, &sync_marker("older00", 1))
    ]);
    let (dir, gh) = stub("sync-spent", &listing, "ok");
    assert_eq!(
        go(&gh, 1),
        RemedyOutcome::Escalated {
            notice_posted: true,
            spent: 3,
            budget: 3
        }
    );
    let log = argv(&dir);
    assert!(!log.contains("update-branch"), "no second sync: {log}");
    assert!(applied(&log, HOLD_LABEL), "{log}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_untrusted_sync_marker_is_ignored() {
    let listing = json!([
        exhausted_chain(),
        comment("drive-by", &sync_marker("abc0000", 1))
    ]);
    let (dir, gh) = stub("sync-untrusted", &listing, "ok");
    assert!(matches!(go(&gh, 1), RemedyOutcome::SyncedBase { n: 1, .. }));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn zero_handoffs_is_todays_behaviour() {
    let (dir, gh) = stub("sync-off", &json!([exhausted_chain()]), "ok");
    assert!(matches!(go(&gh, 0), RemedyOutcome::Escalated { .. }));
    assert!(!argv(&dir).contains("update-branch"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_non_conflict_sync_failure_falls_back_to_the_hold() {
    let (dir, gh) = stub("sync-500", &json!([exhausted_chain()]), "other");
    assert!(matches!(go(&gh, 1), RemedyOutcome::Escalated { .. }));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn config_precedence_env_over_config_over_default() {
    let cfg = json!({"champion": {"redateSyncHandoffs": 2}});
    assert_eq!(SyncConfig::resolve(None, &json!({})).handoffs, DEFAULT_HANDOFFS);
    assert_eq!(SyncConfig::resolve(None, &cfg).handoffs, 2);
    assert_eq!(SyncConfig::resolve(Some("0"), &cfg).handoffs, 0);
    assert_eq!(SyncConfig::resolve(Some("lots"), &cfg).handoffs, 2);
    assert_eq!(SyncConfig::resolve(Some("99"), &cfg).handoffs, MAX_HANDOFFS);
}

#[test]
fn counts_markers_across_heads_not_the_conflict_marker() {
    let bodies =
        format!("{}\n{}\n{}", sync_marker("a", 1), sync_marker("b", 2), conflict_marker("c"));
    assert_eq!(handoffs_used(&bodies), 2);
    let _ = split_stub_calls("");
}
