//! Tests for the chain-head merge lock (#10167).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::forge_identity::{FleetLogins, Roster};
use serde_json::json;

const HEAD: &str = "1111111111111111111111111111111111111111";
const OTHER: &str = "2222222222222222222222222222222222222222";

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn marker(cap: u64) -> LockMarker {
    LockMarker {
        base: "main".into(),
        head: HEAD.into(),
        acquired: at("2026-10-05T12:00:00Z"),
        cap_secs: cap,
    }
}

fn comment(body: &str, created: &str, assoc: &str) -> Value {
    json!({
        "user": {"login": "someone", "type": "User"},
        "author_association": assoc,
        "body": body,
        "created_at": created,
    })
}

fn observed(cap: u64, created: &str) -> ObservedLock {
    ObservedLock {
        marker: marker(cap),
        created_at: at(created),
    }
}

fn holder(number: u32) -> Holder {
    Holder {
        number,
        open: true,
        head_sha: HEAD.into(),
        base_ref: "main".into(),
        labels: vec!["loom:pr".into()],
        updated_at: Some(at("2026-10-05T12:00:00Z")),
    }
}

fn pending() -> Result<bool, String> {
    Ok(false)
}

fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&Roster::default()), None, Vec::new())
}

fn run(name: &str, status: &str, started: &str) -> CheckRun {
    CheckRun {
        name: name.into(),
        status: status.into(),
        conclusion: (status == "completed").then(|| "success".into()),
        started_at: Some(at(started)),
        actions_run_id: None,
        actions_job_id: None,
    }
}

// --- marker -----------------------------------------------------------------

#[test]
fn marker_round_trips_through_the_parser() {
    let m = marker(1200);
    let body = lock_comment_body(&m);
    let lock = newest_lock(&[comment(&body, "2026-10-05T12:00:05Z", "MEMBER")]).unwrap();
    assert_eq!(lock.marker, m);
    assert_eq!(lock.created_at, at("2026-10-05T12:00:05Z"));
    assert_eq!(body.matches(MARKER_PREFIX).count(), 1, "exactly one marker: {body}");
}

#[test]
fn malformed_markers_are_not_markers() {
    for bad in [
        format!("<!-- {MARKER_PREFIX} base=main head=abc acquired=2026-10-05T12:00:00Z cap=60 -->"),
        format!("<!-- {MARKER_PREFIX} base=main head={HEAD} acquired=yesterday cap=60 -->"),
        format!("<!-- {MARKER_PREFIX} base=main head={HEAD} acquired=2026-10-05T12:00:00Z cap=0 -->"),
        format!("<!-- {MARKER_PREFIX} head={HEAD} acquired=2026-10-05T12:00:00Z cap=60 -->"),
        format!("<!-- {MARKER_PREFIX} base=main head={HEAD} acquired=2026-10-05T12:00:00Z cap=60 x=1 -->"),
        format!("<!-- {MARKER_PREFIX}-x base=main head={HEAD} acquired=2026-10-05T12:00:00Z cap=60 -->"),
        format!("`{MARKER_PREFIX} base=main head={HEAD} acquired=2026-10-05T12:00:00Z cap=60`"),
        format!("<!-- {MARKER_PREFIX} base=main head={HEAD} acquired=2026-10-05T12:00:00Z cap=60"),
    ] {
        assert_eq!(newest_lock(&[comment(&bad, "2026-10-05T12:00:00Z", "MEMBER")]), None, "{bad}");
    }
}

#[test]
fn a_huge_marker_cap_is_clamped() {
    let body = format!(
        "<!-- {MARKER_PREFIX} base=main head={HEAD} acquired=2026-10-05T12:00:00Z cap=999999 -->"
    );
    let lock = newest_lock(&[comment(&body, "2026-10-05T12:00:00Z", "MEMBER")]).unwrap();
    assert_eq!(lock.marker.cap_secs, MAX_CAP_SECS);
}

#[test]
fn newest_marker_wins() {
    let mut newer = marker(600);
    newer.head = OTHER.into();
    let listing = [
        comment(&marker_text(&marker(1200)), "2026-10-05T12:00:00Z", "MEMBER"),
        comment("unrelated", "2026-10-05T12:01:00Z", "MEMBER"),
        comment(&marker_text(&newer), "2026-10-05T12:02:00Z", "MEMBER"),
    ];
    let lock = newest_lock(&listing).unwrap();
    assert_eq!(lock.marker.head, OTHER);
    assert_eq!(lock.created_at, at("2026-10-05T12:02:00Z"));
}

#[test]
fn a_comment_without_created_at_carries_no_lock() {
    let c = json!({"author_association": "MEMBER", "body": marker_text(&marker(60))});
    assert_eq!(newest_lock(&[c]), None);
}

#[test]
fn an_outsider_marker_is_filtered_before_parsing() {
    let listing = json!([comment(
        &marker_text(&marker(1200)),
        "2026-10-05T12:00:00Z",
        "NONE"
    ),])
    .to_string();
    let trusted = policy().trusted_listing(listing.as_bytes()).unwrap();
    assert_eq!(newest_lock(&trusted), None, "an outsider's marker must never hold a merge");

    let insider = json!([comment(
        &marker_text(&marker(1200)),
        "2026-10-05T12:00:00Z",
        "MEMBER"
    )])
    .to_string();
    let trusted = policy().trusted_listing(insider.as_bytes()).unwrap();
    assert!(newest_lock(&trusted).is_some());
}

#[test]
fn the_lock_marker_does_not_touch_the_redate_budget() {
    // #9590: acquiring the lock must not spend or refund a re-date.
    let body = lock_comment_body(&marker(1200));
    let listing = [json!({"body": body, "created_at": "2026-10-05T12:00:00Z"})];
    let pos = crate::merge_pr::redate::budget::chain_position(&listing, HEAD);
    assert_eq!(pos.spent, 0);
    assert_eq!(pos.recorded_at, None);
}

// --- cap resolution ----------------------------------------------------------

#[test]
fn cap_resolves_env_then_config_then_default_with_clamps() {
    let none = json!({});
    let cfg = json!({"champion": {"chainLockCapSecs": 900}});
    assert_eq!(resolve_cap(None, &none), DEFAULT_CAP_SECS);
    assert_eq!(resolve_cap(None, &cfg), 900);
    assert_eq!(resolve_cap(Some("300"), &cfg), 300, "env beats config");
    assert_eq!(resolve_cap(Some(" 300 "), &none), 300);
    for invalid in ["0", "-5", "abc", ""] {
        assert_eq!(resolve_cap(Some(invalid), &cfg), 900, "{invalid:?} falls through");
        assert_eq!(resolve_cap(Some(invalid), &none), DEFAULT_CAP_SECS, "{invalid:?}");
    }
    assert_eq!(resolve_cap(Some("99999999"), &none), MAX_CAP_SECS, "huge env is clamped");
    let huge = json!({"champion": {"chainLockCapSecs": 99_999_999}});
    assert_eq!(resolve_cap(None, &huge), MAX_CAP_SECS, "huge config is clamped");
    let zero = json!({"champion": {"chainLockCapSecs": 0}});
    assert_eq!(resolve_cap(None, &zero), DEFAULT_CAP_SECS);
}

#[test]
fn override_parses_falsey_values_as_off() {
    for on in ["1", "true", "yes", "on"] {
        assert!(override_set(Some(on)), "{on}");
    }
    for off in ["", "0", "false", "no", "off", "FALSE"] {
        assert!(!override_set(Some(off)), "{off}");
    }
    assert!(!override_set(None));
}

// --- liveness ----------------------------------------------------------------

#[test]
fn a_fresh_lock_with_pending_checks_is_live() {
    let lock = observed(1200, "2026-10-05T12:00:00Z");
    let got = evaluate(&lock, &holder(9), 1200, at("2026-10-05T12:05:00Z"), pending).unwrap();
    assert_eq!(
        got,
        Liveness::Live {
            expires_at: at("2026-10-05T12:20:00Z")
        }
    );
}

#[test]
fn a_lock_expires_at_the_cap() {
    let lock = observed(1200, "2026-10-05T12:00:00Z");
    for now in ["2026-10-05T12:20:00Z", "2026-10-05T13:00:00Z"] {
        let got = evaluate(&lock, &holder(9), 1200, at(now), pending).unwrap();
        assert_eq!(got, Liveness::Expired(Expiry::Cap), "{now}");
    }
}

#[test]
fn the_cap_is_measured_from_created_at_not_the_embedded_timestamp() {
    let mut lock = observed(1200, "2026-10-05T12:00:00Z");
    lock.marker.acquired = at("2030-01-01T00:00:00Z"); // forged / skewed
    let got = evaluate(&lock, &holder(9), 1200, at("2026-10-05T12:30:00Z"), pending).unwrap();
    assert_eq!(got, Liveness::Expired(Expiry::Cap));
}

#[test]
fn the_smaller_of_the_marker_and_reader_caps_applies() {
    let lock = observed(1200, "2026-10-05T12:00:00Z");
    let now = at("2026-10-05T12:06:00Z");
    assert_eq!(
        evaluate(&lock, &holder(9), 300, now, pending).unwrap(),
        Liveness::Expired(Expiry::Cap)
    );
    let short = observed(300, "2026-10-05T12:00:00Z");
    assert_eq!(
        evaluate(&short, &holder(9), 1200, now, pending).unwrap(),
        Liveness::Expired(Expiry::Cap)
    );
}

#[test]
fn a_head_move_voids_the_lock() {
    let mut h = holder(9);
    h.head_sha = OTHER.into();
    let lock = observed(1200, "2026-10-05T12:00:00Z");
    let got = evaluate(&lock, &h, 1200, at("2026-10-05T12:01:00Z"), pending).unwrap();
    assert_eq!(got, Liveness::Expired(Expiry::HeadMoved));
}

#[test]
fn a_failed_required_check_releases_the_lock() {
    let lock = observed(1200, "2026-10-05T12:00:00Z");
    let got = evaluate(&lock, &holder(9), 1200, at("2026-10-05T12:01:00Z"), || Ok(true)).unwrap();
    assert_eq!(got, Liveness::Expired(Expiry::ChecksFailed));
}

#[test]
fn green_checks_keep_a_follower_held_until_the_head_lands_or_the_cap_ends_it() {
    // #10448: every required check is green (none failed), yet the lock holds.
    let lock = observed(1200, "2026-10-05T12:00:00Z");
    let held = |now: &str, h: &Holder| evaluate(&lock, h, 1200, at(now), pending).unwrap();
    let live = Liveness::Live {
        expires_at: at("2026-10-05T12:20:00Z"),
    };
    assert_eq!(held("2026-10-05T12:01:00Z", &holder(9)), live);
    assert_eq!(held("2026-10-05T12:19:59Z", &holder(9)), live, "still held just before the cap");
    // The head merging closes it: released.
    let mut merged = holder(9);
    merged.open = false;
    assert_eq!(held("2026-10-05T12:10:00Z", &merged), Liveness::Expired(Expiry::Closed));
    // A moved head releases early.
    let mut moved = holder(9);
    moved.head_sha = OTHER.into();
    assert_eq!(held("2026-10-05T12:10:00Z", &moved), Liveness::Expired(Expiry::HeadMoved));
    // The cap releases a head that never lands.
    assert_eq!(held("2026-10-05T12:20:00Z", &holder(9)), Liveness::Expired(Expiry::Cap));
}

#[test]
fn a_closed_or_merged_holder_releases_the_lock() {
    let mut h = holder(9);
    h.open = false;
    let lock = observed(1200, "2026-10-05T12:00:00Z");
    let got =
        evaluate(&lock, &h, 1200, at("2026-10-05T12:01:00Z"), || panic!("no check read")).unwrap();
    assert_eq!(got, Liveness::Expired(Expiry::Closed));
}

#[test]
fn an_escalated_holder_releases_the_lock() {
    let mut h = holder(9);
    h.labels.push("loom:operator".into());
    let lock = observed(1200, "2026-10-05T12:00:00Z");
    let got = evaluate(&lock, &h, 1200, at("2026-10-05T12:01:00Z"), pending).unwrap();
    assert_eq!(got, Liveness::Expired(Expiry::Escalated));
}

#[test]
fn a_retargeted_holder_releases_the_lock() {
    let mut h = holder(9);
    h.base_ref = "release".into();
    let lock = observed(1200, "2026-10-05T12:00:00Z");
    let got = evaluate(&lock, &h, 1200, at("2026-10-05T12:01:00Z"), pending).unwrap();
    assert_eq!(got, Liveness::Expired(Expiry::OtherBase));
}

#[test]
fn checks_are_read_only_for_an_otherwise_live_lock_and_a_failed_read_is_unreadable() {
    let lock = observed(1200, "2026-10-05T12:00:00Z");
    let expired = evaluate(&lock, &holder(9), 1200, at("2026-10-05T13:00:00Z"), || {
        panic!("an expired lock needs no check read")
    });
    assert_eq!(expired.unwrap(), Liveness::Expired(Expiry::Cap));
    let err = evaluate(&lock, &holder(9), 1200, at("2026-10-05T12:01:00Z"), || {
        Err("rate limited".into())
    });
    assert_eq!(err, Err("rate limited".to_string()));
}

fn run_with(name: &str, status: &str, conclusion: Option<&str>, started: &str) -> CheckRun {
    CheckRun {
        conclusion: conclusion.map(Into::into),
        ..run(name, status, started)
    }
}

#[test]
fn checks_failed_is_red_only_for_a_completed_non_success_latest_run() {
    let req = vec!["build".to_string(), "test".to_string()];
    let green = [
        run("build", "completed", "2026-10-05T12:00:00Z"),
        run("test", "completed", "2026-10-05T12:00:00Z"),
    ];
    assert!(!checks_failed(&req, &green), "all green is not red");
    let red = [
        green[0].clone(),
        run_with("test", "completed", Some("failure"), "2026-10-05T12:00:00Z"),
    ];
    assert!(checks_failed(&req, &red));
    let rerun = [
        run_with("test", "completed", Some("failure"), "2026-10-05T11:00:00Z"),
        run("test", "in_progress", "2026-10-05T12:00:00Z"),
    ];
    assert!(!checks_failed(&req, &rerun), "the latest run of `test` is still running");
    assert!(!checks_failed(&req, &green[..1]), "a missing context has not failed");
    assert!(!checks_failed(&[], &[]), "no runs yet has not failed");
    assert!(checks_failed(&[], &red));
    for c in ["neutral", "skipped"] {
        assert!(!checks_failed(
            &[],
            &[run_with("x", "completed", Some(c), "2026-10-05T12:00:00Z")]
        ));
    }
    for c in ["cancelled", "timed_out", "action_required"] {
        assert!(checks_failed(
            &[],
            &[run_with("x", "completed", Some(c), "2026-10-05T12:00:00Z")]
        ));
    }
}

// --- guard decision -----------------------------------------------------------

fn live(holder: u32, created: &str) -> LiveLock {
    LiveLock {
        holder,
        head: HEAD.into(),
        created_at: at(created),
        expires_at: at(created) + TimeDelta::seconds(1200),
    }
}

#[test]
fn another_prs_live_lock_holds_the_merge() {
    let l = live(9, "2026-10-05T12:00:00Z");
    assert_eq!(decide(7, std::slice::from_ref(&l)), Guard::Held(l));
}

#[test]
fn the_chain_head_is_never_held_by_its_own_lock() {
    assert_eq!(decide(9, &[live(9, "2026-10-05T12:00:00Z")]), Guard::Clear);
    assert_eq!(decide(7, &[]), Guard::Clear);
}

#[test]
fn two_locked_heads_cannot_hold_each_other() {
    let older = live(9, "2026-10-05T12:00:00Z");
    let newer = live(5, "2026-10-05T12:03:00Z");
    let both = [older.clone(), newer];
    assert_eq!(decide(9, &both), Guard::Clear, "the oldest lock goes first");
    assert_eq!(decide(5, &both), Guard::Held(older.clone()));
    assert_eq!(decide(7, &both), Guard::Held(older), "a bystander names the oldest");
}

// --- unreadable state -----------------------------------------------------------

#[test]
fn unreadable_state_defers_within_the_cap_and_fails_open_after_it() {
    let first = at("2026-10-05T12:00:00Z");
    assert_eq!(
        decide_unreadable(first, 1200, at("2026-10-05T12:10:00Z")),
        Unreadable::Defer {
            until: at("2026-10-05T12:20:00Z")
        }
    );
    assert_eq!(
        decide_unreadable(first, 1200, at("2026-10-05T12:20:00Z")),
        Unreadable::FailOpen { since: first }
    );
}

#[test]
fn the_first_unreadable_read_is_recorded_once_and_cleared_by_a_good_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = unreadable_state_path(dir.path(), "release/1.x");
    assert!(path.ends_with("unreadable-release_1.x.json"), "{}", path.display());
    let first = at("2026-10-05T12:00:00Z");
    assert_eq!(note_unreadable(&path, first, 1200), Some(first));
    assert_eq!(
        note_unreadable(&path, at("2026-10-05T12:15:00Z"), 1200),
        Some(first),
        "kept, not reset"
    );
    clear_unreadable(&path);
    let later = at("2026-10-05T13:00:00Z");
    assert_eq!(note_unreadable(&path, later, 1200), Some(later));
}

#[test]
fn a_record_older_than_twice_the_cap_counts_as_a_fresh_first_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = unreadable_state_path(dir.path(), "main");
    let first = at("2026-10-05T12:00:00Z");
    assert_eq!(note_unreadable(&path, first, 1200), Some(first));
    // 2x cap is 2400 s: just inside, the record still stands.
    let inside = at("2026-10-05T12:39:59Z");
    assert_eq!(note_unreadable(&path, inside, 1200), Some(first));
    // At 2x the cap it is a leftover: the failure is fresh, so it defers.
    let later = at("2026-10-05T13:00:00Z");
    let got = note_unreadable(&path, later, 1200).unwrap();
    assert_eq!(got, later);
    assert!(matches!(decide_unreadable(got, 1200, later), Unreadable::Defer { .. }));
}

#[test]
fn an_unwritable_record_reports_none() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join(".loom");
    std::fs::write(&blocker, "a file, not a directory").unwrap();
    let path = unreadable_state_path(dir.path(), "main");
    assert_eq!(note_unreadable(&path, at("2026-10-05T12:00:00Z"), 1200), None);
}

// --- forge I/O against a stub `gh` --------------------------------------------

/// A stub `gh`, logging argv to `dir/argv.log`. The repo-wide comment listing
/// (`api --include repos/o/r/issues/comments?…`) answers page 1 from
/// `dir/comments.json` and page N (`&page=N`) from `dir/comments-pN.json`
/// (`[]` when that page file is absent), with an ETag derived from the page's
/// content, or a 304 when `If-None-Match` carries it. `pulls/N` answers from
/// `dir/pull-N.json`. A missing file is a failed read.
fn stub_gh(dir: &Path) -> String {
    let path = dir.join("gh");
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{d}/argv.log"
if [ "$2" = "--include" ]; then
  [ -f "{d}/comments.json" ] || exit 1
  page=1
  [[ "$3" =~ \&page=([0-9]+) ]] && page="${{BASH_REMATCH[1]}}"
  if [ "$page" = 1 ]; then body="$(cat "{d}/comments.json")"
  elif [ -f "{d}/comments-p$page.json" ]; then body="$(cat "{d}/comments-p$page.json")"
  else body='[]'; fi
  tag="\"p$page-$(printf '%s' "$body" | cksum | cut -d' ' -f1)\""
  if [[ "$*" == *"If-None-Match: $tag"* ]]; then
    printf 'HTTP/2.0 304 Not Modified\netag: %s\n\n' "$tag"
  else
    printf 'HTTP/2.0 200 OK\netag: %s\n\n' "$tag"
    printf '%s' "$body"
  fi
  exit 0
fi
case "$2" in
  repos/o/r/pulls/*) n="${{2#repos/o/r/pulls/}}"; f="{d}/pull-${{n%%/*}}.json" ;;
  repos/o/r/commits/*/check-runs*) f="{d}/runs.json" ;;
  repos/o/r/rules/branches/*) exit 0 ;;
  graphql) exit 0 ;;
  *) exit 2 ;;
esac
[ -f "$f" ] || exit 1
cat "$f"
"#,
        d = dir.display()
    );
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path.to_string_lossy().into_owned()
}

fn pull(number: u32, base: &str) -> Value {
    json!({
        "number": number, "state": "open",
        "head": {"sha": HEAD}, "base": {"ref": base},
        "labels": [{"name": "loom:pr"}],
        "updated_at": "2026-10-05T12:00:30Z",
    })
}

/// A lock comment on PR `n`, as the repo-wide listing returns it.
fn lock_on(n: u32, created: &str) -> Value {
    let mut c = comment(&lock_comment_body(&marker(1200)), created, "MEMBER");
    c["issue_url"] = json!(format!("https://api.github.com/repos/o/r/issues/{n}"));
    c
}

fn guard_with(dir: &Path, pr: u32, base: &str, now: &str) -> Result<Guard, String> {
    let gh = stub_gh(dir);
    let policy = policy();
    let cache = dir.join("etag-cache");
    read_guard(&GuardInputs {
        gh: &gh,
        root: dir,
        nwo: "o/r",
        pr,
        base,
        cap_secs: 1200,
        now: at(now),
        policy: &policy,
        cache_dir: Some(&cache),
    })
}

/// Seed the listing as the forge pages it: 100 comments per page.
fn seed(dir: &Path, pulls: &[Value], comments: &[Value]) {
    for n in 2..=10 {
        let _ = std::fs::remove_file(dir.join(format!("comments-p{n}.json")));
    }
    let mut pages = comments.chunks(COMMENT_PAGE);
    let first = pages.next().unwrap_or_default();
    std::fs::write(dir.join("comments.json"), Value::Array(first.to_vec()).to_string()).unwrap();
    for (n, page) in pages.enumerate() {
        std::fs::write(
            dir.join(format!("comments-p{}.json", n + 2)),
            Value::Array(page.to_vec()).to_string(),
        )
        .unwrap();
    }
    for p in pulls {
        std::fs::write(dir.join(format!("pull-{}.json", p["number"])), p.to_string()).unwrap();
    }
    let runs = json!({"total_count": 1, "check_runs": [
        {"name": "build", "status": "in_progress", "conclusion": null, "started_at": "2026-10-05T12:00:40Z"}
    ]});
    std::fs::write(dir.join("runs.json"), runs.to_string()).unwrap();
}

fn argv_log(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("argv.log")).unwrap_or_default()
}

#[test]
fn read_guard_holds_another_pr_behind_a_live_lock() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), &[pull(9, "main")], &[lock_on(9, "2026-10-05T12:00:20Z")]);
    let got = guard_with(dir.path(), 7, "main", "2026-10-05T12:05:00Z").unwrap();
    let Guard::Held(l) = got else {
        panic!("expected a hold, got {got:?}")
    };
    assert_eq!(l.holder, 9);
    assert_eq!(l.expires_at, at("2026-10-05T12:20:20Z"));
    let argv = argv_log(dir.path());
    assert!(argv.contains("issues/comments?since=2026-10-05T11%3A45%3A00Z"), "{argv}");
    assert!(
        !argv.contains("-X") && !argv.contains("POST"),
        "the guard writes nothing: {argv}"
    );
}

#[test]
fn read_guard_keeps_holding_after_the_heads_checks_are_all_green() {
    // #10448: the head's required check completed green; the follower is still held.
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), &[pull(9, "main")], &[lock_on(9, "2026-10-05T12:00:20Z")]);
    let runs = json!({"total_count": 1, "check_runs": [
        {"name": "build", "status": "completed", "conclusion": "success", "started_at": "2026-10-05T12:00:40Z"}
    ]});
    std::fs::write(dir.path().join("runs.json"), runs.to_string()).unwrap();
    let got = guard_with(dir.path(), 7, "main", "2026-10-05T12:10:00Z").unwrap();
    assert!(matches!(got, Guard::Held(ref l) if l.holder == 9), "{got:?}");
    // A red check releases it.
    let runs = json!({"total_count": 1, "check_runs": [
        {"name": "build", "status": "completed", "conclusion": "failure", "started_at": "2026-10-05T12:00:40Z"}
    ]});
    std::fs::write(dir.path().join("runs.json"), runs.to_string()).unwrap();
    assert_eq!(guard_with(dir.path(), 7, "main", "2026-10-05T12:10:00Z").unwrap(), Guard::Clear);
    // A head that merged (closed) releases it.
    let mut closed = pull(9, "main");
    closed["state"] = json!("closed");
    seed(dir.path(), &[closed], &[lock_on(9, "2026-10-05T12:00:20Z")]);
    assert_eq!(guard_with(dir.path(), 7, "main", "2026-10-05T12:10:00Z").unwrap(), Guard::Clear);
}

/// The recorded cost so far: `(chain_lock.comments ok, its 304s, core consumed)`.
fn cost_so_far() -> (u64, u64, u64) {
    let report = crate::forge_call_stats::status_report(Utc::now(), None);
    let rows = report.host_window.expect("sink enabled on this thread");
    let (ok, not_modified) = rows
        .iter()
        .find(|r| r.caller == "chain_lock.comments")
        .map_or((0, 0), |r| (r.ok, r.not_modified));
    let core = report
        .own_window
        .unwrap()
        .iter()
        .filter(|r| r.pool == "core")
        .map(|r| r.consumed)
        .sum();
    (ok, not_modified, core)
}

/// Run a no-lock check, then an unchanged re-check, recording the forge cost
/// of each: `[(ok, 304s, core) after the first, after the second]`.
fn check_twice_costed(dir: &Path) -> [(u64, u64, u64); 2] {
    let sink = dir.join("sink");
    std::fs::create_dir(&sink).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sink, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    crate::forge_call_stats::set_test_sink_dir(Some(sink));
    let first = guard_with(dir, 7, "main", "2026-10-05T12:05:00Z");
    let after_first = cost_so_far();
    let second = guard_with(dir, 7, "main", "2026-10-05T12:05:30Z");
    let after_second = cost_so_far();
    crate::forge_call_stats::set_test_sink_dir(None);
    assert_eq!(first.unwrap(), Guard::Clear);
    assert_eq!(second.unwrap(), Guard::Clear);
    [after_first, after_second]
}

fn chatter(n: usize) -> Vec<Value> {
    (0..n)
        .map(|k| {
            let mut c = comment("just a review note", "2026-10-05T12:00:20Z", "MEMBER");
            c["id"] = json!(1000 + k);
            c["issue_url"] = json!(format!("https://api.github.com/repos/o/r/issues/{}", 100 + k));
            c
        })
        .collect()
}

#[test]
fn a_lock_check_with_no_lock_is_one_billable_call_and_an_unchanged_recheck_is_a_304() {
    let dir = tempfile::tempdir().unwrap();
    // Unrelated comments, but no lock marker anywhere.
    seed(dir.path(), &[pull(7, "main")], &chatter(1));
    let [first, second] = check_twice_costed(dir.path());
    assert_eq!(first, (1, 0, 1), "the whole check costs one billable call");
    assert_eq!(second, (1, 1, 1), "the unchanged re-check is a free 304");
    let argv = argv_log(dir.path());
    assert!(
        !argv.contains("pulls/") && !argv.contains("check-runs"),
        "nothing else read: {argv}"
    );
    assert!(argv.contains("If-None-Match"), "the re-check is conditional: {argv}");
}

#[test]
fn a_crowded_window_with_no_lock_bills_once_per_page_and_rechecks_free() {
    // #10448 review: >=100 unrelated comments must not add an unconditional
    // paginated read on every check, nor a billed one on an unchanged re-check.
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), &[pull(7, "main")], &chatter(150));
    let [first, second] = check_twice_costed(dir.path());
    assert_eq!(first, (2, 0, 2), "two pages, one billable call each");
    assert_eq!(second, (2, 2, 2), "the unchanged re-check is two free 304s");
    let argv = argv_log(dir.path());
    assert!(!argv.contains("--paginate"), "no unconditional listing: {argv}");
    assert!(!argv.contains("pulls/"), "no holder read without a marker: {argv}");
    assert_eq!(argv.matches("If-None-Match").count(), 2, "{argv}");
}

#[test]
fn exactly_one_full_page_reads_the_empty_page_behind_it() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), &[pull(7, "main")], &chatter(100));
    let [first, second] = check_twice_costed(dir.path());
    assert_eq!(first, (2, 0, 2));
    assert_eq!(second, (2, 2, 2));
}

#[test]
fn a_new_comment_rebills_only_the_last_page() {
    let dir = tempfile::tempdir().unwrap();
    let mut comments = chatter(150);
    seed(dir.path(), &[pull(7, "main")], &comments);
    let [first, _] = check_twice_costed(dir.path());
    assert_eq!(first.0, 2);
    // A new unrelated comment appends to page 2; page 1 is still a 304.
    comments.extend(chatter(151).into_iter().skip(150));
    seed(dir.path(), &[pull(7, "main")], &comments);
    std::fs::remove_dir_all(dir.path().join("sink")).unwrap();
    let [third, _] = check_twice_costed(dir.path());
    assert_eq!(third, (1, 1, 1), "page 1 unchanged (304), page 2 billed");
}

#[test]
fn a_lock_beyond_the_first_page_still_holds() {
    let dir = tempfile::tempdir().unwrap();
    let mut comments = chatter(230);
    comments.push(lock_on(9, "2026-10-05T12:00:20Z"));
    seed(dir.path(), &[pull(9, "main")], &comments);
    let got = guard_with(dir.path(), 7, "main", "2026-10-05T12:05:00Z").unwrap();
    assert!(matches!(got, Guard::Held(ref l) if l.holder == 9), "{got:?}");
    let argv = argv_log(dir.path());
    assert!(argv.contains("&page=3"), "the walk reaches the marker's page: {argv}");
}

#[test]
fn a_failed_later_page_is_unreadable_not_a_truncated_clear() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), &[pull(7, "main")], &chatter(150));
    std::fs::write(dir.path().join("comments-p2.json"), "not json").unwrap();
    assert!(guard_with(dir.path(), 7, "main", "2026-10-05T12:05:00Z").is_err());
}

#[test]
fn read_guard_does_not_hold_the_chain_head_itself() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), &[pull(9, "main")], &[lock_on(9, "2026-10-05T12:00:20Z")]);
    assert_eq!(guard_with(dir.path(), 9, "main", "2026-10-05T12:05:00Z").unwrap(), Guard::Clear);
}

#[test]
fn read_guard_clears_past_the_cap_without_reading_the_holder() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), &[pull(9, "main")], &[lock_on(9, "2026-10-05T12:00:20Z")]);
    assert_eq!(guard_with(dir.path(), 7, "main", "2026-10-05T12:30:00Z").unwrap(), Guard::Clear);
    let argv = argv_log(dir.path());
    assert!(!argv.contains("pulls/9"), "an expired lock's holder is not read: {argv}");
}

#[test]
fn read_guard_ignores_a_lock_on_another_base() {
    let dir = tempfile::tempdir().unwrap();
    // The marker is for `main`; a merge onto `release` is not held by it.
    seed(dir.path(), &[pull(9, "main")], &[lock_on(9, "2026-10-05T12:00:20Z")]);
    assert_eq!(
        guard_with(dir.path(), 7, "release", "2026-10-05T12:05:00Z").unwrap(),
        Guard::Clear
    );
    assert!(!argv_log(dir.path()).contains("pulls/9"));
}

#[test]
fn read_guard_reports_a_failed_read_as_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path(), &[pull(9, "main")], &[lock_on(9, "2026-10-05T12:00:20Z")]);
    std::fs::remove_file(dir.path().join("pull-9.json")).unwrap();
    assert!(guard_with(dir.path(), 7, "main", "2026-10-05T12:05:00Z").is_err());
    std::fs::remove_file(dir.path().join("comments.json")).unwrap();
    assert!(guard_with(dir.path(), 7, "main", "2026-10-05T12:05:00Z").is_err());
}

#[test]
fn record_lock_posts_exactly_one_marker_for_the_prs_own_base() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    let script = format!(
        r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> "{d}/argv.log"
prev=""
for a in "$@"; do [ "$prev" = "--input" ] && cat -- "$a" >> "{d}/bodies.log"; prev="$a"; done
case "$2" in
  repos/o/r/pulls/9) echo '{{"base": {{"ref": "main"}}}}' ;;
  *) echo '{{"id": 1}}' ;;
esac
"#
    );
    let gh = dir.path().join("gh");
    std::fs::write(&gh, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let now = at("2026-10-05T12:00:00Z");
    let m = record_lock(gh.to_str().unwrap(), dir.path(), "o/r", "9", HEAD, 1200, now).unwrap();
    assert_eq!(m, marker(1200));
    let bodies = std::fs::read_to_string(dir.path().join("bodies.log")).unwrap();
    assert_eq!(bodies.matches(MARKER_PREFIX).count(), 1, "{bodies}");
    assert!(bodies.contains(&marker_text(&m)), "{bodies}");
    assert!(!bodies.contains("loom:stale-check-redate"), "no budget marker: {bodies}");
    let argv = std::fs::read_to_string(dir.path().join("argv.log")).unwrap();
    assert_eq!(argv.lines().count(), 2, "one base read, one comment: {argv}");

    // A short SHA is never pinned.
    assert!(record_lock(gh.to_str().unwrap(), dir.path(), "o/r", "9", "abc", 1200, now).is_err());
}
