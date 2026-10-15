//! Tests for the Doctor's activity guard (#12105). The decisions are pure and
//! exercised directly. `claim` is then driven end to end against a fake `gh`
//! that answers the PR's comments, its closing issue's comments, the PR read
//! and the branch activity read separately.

use super::super::{Common, Ctx, Gh, Identity, Role, Row, EXIT_HELD};
use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

use loom_daemon::comment_trust::TrustPolicy;
use loom_daemon::forge_identity::{FleetLogins, Roster};

const SELF_LOGIN: &str = "loom-fleet-dispatch[bot]";
const HOST: &str = "host-a1b2c3d4";

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-15T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn ttl() -> chrono::Duration {
    chrono::Duration::minutes(15)
}

fn who(role: Role, sweep: &str) -> Identity {
    Identity {
        role,
        host: HOST.into(),
        sweep: sweep.into(),
        step: None,
    }
}

fn pr_row(id: u64, age_min: i64, holder: &Identity) -> Row {
    Row {
        id,
        updated_at: now() - chrono::Duration::minutes(age_min),
        body: format!("{}\nprose", holder.lease_marker()),
    }
}

fn issue_marker(host: &str, sweep: &str) -> String {
    format!("<!-- loom:lease host={host} sweep={sweep} -->")
}

fn issue_row(age_min: i64, first_line: String) -> LeaseRow {
    LeaseRow {
        updated_at: now() - chrono::Duration::minutes(age_min),
        body: format!("{first_line}\nprose"),
    }
}

fn push(age_min: i64, login: &str) -> Push {
    Push {
        at: now() - chrono::Duration::minutes(age_min),
        login: login.into(),
    }
}

fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&Roster::default()), Some(SELF_LOGIN.into()), Vec::new())
}

// ---- pure decisions -------------------------------------------------------

#[test]
fn another_roles_live_pr_lease_counts_and_a_doctor_peer_is_left_to_the_claim() {
    let me = who(Role::Doctor, "sweep-me");
    let judge = who(Role::Judge, "sweep-j");
    let doctor = who(Role::Doctor, "sweep-d");
    let rows = [pr_row(10, 1, &doctor), pr_row(20, 2, &judge)];
    let found = other_role_leases(&rows, &me, now(), ttl());
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].holder, judge);
    // A stale one, or a released one (its row rewritten in place), is not
    // activity.
    let stale = [pr_row(20, 16, &judge)];
    assert!(other_role_leases(&stale, &me, now(), ttl()).is_empty());
    let released = [Row {
        id: 20,
        updated_at: now(),
        body: judge.released_marker(),
    }];
    assert!(other_role_leases(&released, &me, now(), ttl()).is_empty());
}

#[test]
fn a_live_issue_lease_counts_unless_it_is_this_claimants_own() {
    let me = who(Role::Doctor, "sweep-me");
    let rows = [issue_row(3, issue_marker("host-other", "sweep-attended"))];
    assert_eq!(
        foreign_issue_leases(&rows, &me, now(), ttl()),
        vec![("host-other".to_string(), "sweep-attended".to_string())]
    );
    // The same (host, sweep) is this session's own builder lease.
    let own = [issue_row(3, issue_marker(HOST, "sweep-me"))];
    assert!(foreign_issue_leases(&own, &me, now(), ttl()).is_empty());
    // The same sweep id on another host is someone else.
    let other_host = [issue_row(3, issue_marker("host-other", "sweep-me"))];
    assert_eq!(foreign_issue_leases(&other_host, &me, now(), ttl()).len(), 1);
    // Aged out, or yielded, is not live.
    let stale = [issue_row(20, issue_marker("host-other", "s"))];
    assert!(foreign_issue_leases(&stale, &me, now(), ttl()).is_empty());
    let yielded = [
        issue_row(3, issue_marker("host-other", "s")),
        issue_row(
            2,
            "<!-- loom:lease-yield host=host-other sweep=s earliest_host=h earliest_sweep=e -->"
                .to_string(),
        ),
    ];
    assert!(foreign_issue_leases(&yielded, &me, now(), ttl()).is_empty());
    // Our own lease is set aside without hiding an older foreign one.
    let both = [
        issue_row(1, issue_marker(HOST, "sweep-me")),
        issue_row(5, issue_marker("host-other", "s")),
    ];
    assert_eq!(foreign_issue_leases(&both, &me, now(), ttl()).len(), 1);
}

#[test]
fn a_recent_push_by_another_login_counts_and_our_own_or_an_old_one_does_not() {
    let policy = policy();
    let is_self = |l: &str| policy.is_self(l);
    let window = chrono::Duration::minutes(15);
    let pushes = [push(3, "rjwalters"), push(1, SELF_LOGIN)];
    assert_eq!(
        recent_foreign_push(&pushes, now(), window, is_self).map(|p| p.login.as_str()),
        Some("rjwalters")
    );
    assert_eq!(recent_foreign_push(&[push(1, SELF_LOGIN)], now(), window, is_self), None);
    assert_eq!(recent_foreign_push(&[push(16, "rjwalters")], now(), window, is_self), None);
    // Clock skew: a push stamped in the future is recent.
    assert!(recent_foreign_push(&[push(-2, "rjwalters")], now(), window, is_self).is_some());
    // With no self identity resolved, every login is another actor.
    let anonymous = TrustPolicy::new(FleetLogins::of(&Roster::default()), None, Vec::new());
    let is_anon = |l: &str| anonymous.is_self(l);
    assert!(recent_foreign_push(&[push(1, SELF_LOGIN)], now(), window, is_anon).is_some());
}

#[test]
fn self_is_matched_by_name_and_kind() {
    let p = policy();
    assert!(p.is_self("loom-fleet-dispatch[bot]"));
    assert!(p.is_self("LOOM-FLEET-DISPATCH[bot]"));
    assert!(!p.is_self("loom-fleet-dispatch"), "a user of the App's name is not the App");
    assert!(!p.is_self("rjwalters"));
    assert!(!p.is_self(""));
}

#[test]
fn the_push_window_defaults_to_fifteen_minutes_and_zero_turns_it_off() {
    let fifteen = Some(chrono::Duration::minutes(15));
    assert_eq!(push_window(None), fifteen);
    assert_eq!(push_window(Some("")), fifteen);
    assert_eq!(push_window(Some("soon")), fifteen);
    assert_eq!(push_window(Some(" 30 ")), Some(chrono::Duration::minutes(30)));
    assert_eq!(push_window(Some("0")), None);
    assert_eq!(push_window(Some("-5")), None);
}

#[test]
fn the_head_and_the_activity_records_parse() {
    let same = serde_json::json!({"branch": "feature/issue-7", "head_repo": "Acme/W", "base_repo": "acme/w"});
    assert_eq!(
        parse_head(&same),
        Some(Head {
            branch: "feature/issue-7".into(),
            same_repo: true
        })
    );
    let fork =
        serde_json::json!({"branch": "main", "head_repo": "someone/w", "base_repo": "acme/w"});
    assert_eq!(parse_head(&fork).map(|h| h.same_repo), Some(false));
    // A deleted fork reports no head repo: not ours to check.
    let gone = serde_json::json!({"branch": "x", "head_repo": null, "base_repo": "acme/w"});
    assert_eq!(parse_head(&gone).map(|h| h.same_repo), Some(false));
    assert_eq!(parse_head(&serde_json::json!({"branch": ""})), None);

    let ndjson = "{\"timestamp\":\"2026-10-15T11:58:00Z\",\"login\":\"rjwalters\"}\n\
                  not json\n\
                  {\"timestamp\":\"2026-10-15T11:50:00Z\",\"login\":\"\"}\n";
    assert_eq!(parse_pushes(ndjson), vec![push(2, "rjwalters")]);
    assert_eq!(query_escape("refs/heads/feature/a b&c"), "refs/heads/feature/a%20b%26c");
}

// ---- claim end to end -----------------------------------------------------

const AUTHOR: &str =
    r#""user":{"login":"loom-fleet-dispatch[bot]","type":"Bot"},"author_association":"NONE""#;
const PR: u64 = 77;
const STORY: u64 = 12;

/// A fake `gh`. `<dir>/pr_rows` and `<dir>/issue_rows` are PR #77's and
/// issue #12's comments (NDJSON, unfiltered: the parsers ignore other
/// markers). `<dir>/head` answers the head-branch read and `<dir>/body` the
/// PR-body read. `<dir>/pushes` answers the activity read, which is logged in
/// `<dir>/activity`; without the file it fails. A POST is logged in
/// `<dir>/posted` and appended to the PR's comments.
fn fake_gh(dir: &Path) -> PathBuf {
    let d = dir.display();
    let script = format!(
        r#"#!/usr/bin/env bash
D="{d}"
case " $* " in
  *" POST "*)
    prev=""; body=""
    for a in "$@"; do [ "$prev" = "-f" ] && body="${{a#body=}}"; prev="$a"; done
    first="${{body%%$'\n'*}}"
    id=$(cat "$D/next"); echo $((id + 1)) > "$D/next"
    printf '{{"id":%s,"updated_at":"%s","body":"%s",%s}}\n' "$id" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$first" '{AUTHOR}' >> "$D/pr_rows"
    echo "$first" >> "$D/posted"
    echo "$id" ;;
  *"/activity?"*) echo "$*" >> "$D/activity"; [ -f "$D/pushes" ] || exit 1; cat "$D/pushes" ;;
  *"/issues/{PR}/comments"*) cat "$D/pr_rows" ;;
  *"/issues/{STORY}/comments"*) cat "$D/issue_rows" ;;
  *"head.ref"*) cat "$D/head" 2>/dev/null || exit 1 ;;
  *"/pulls/{PR}"*) cat "$D/body" 2>/dev/null || echo "" ;;
  *) exit 1 ;;
esac
"#
    );
    let path = dir.join("gh");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(dir.join("pr_rows"), "").unwrap();
    fs::write(dir.join("issue_rows"), "").unwrap();
    fs::write(dir.join("next"), "500").unwrap();
    fs::write(dir.join("body"), format!("Fixes the thing.\n\nCloses #{STORY}\n")).unwrap();
    fs::write(
        dir.join("head"),
        r#"{"branch":"feature/issue-12","head_repo":"acme/widgets","base_repo":"acme/widgets"}"#,
    )
    .unwrap();
    path
}

fn ctx(dir: &Path, me: Identity) -> Ctx {
    Ctx {
        common: Common {
            pr: PR,
            role: me.role,
            step: me.step,
            watch_pid: None,
            sweep_id: Some(me.sweep.clone()),
            workspace: dir.to_path_buf(),
        },
        me,
        gh: Gh {
            bin: dir.join("gh"),
            root: dir.to_path_buf(),
            write_repo: Ok("acme/widgets".to_string()),
        },
        policy: policy(),
        ttl: ttl(),
        now: Utc::now,
        manage_renewer: false,
        lease_file: None,
    }
}

fn stamp_ago(min: i64) -> String {
    (Utc::now() - chrono::Duration::minutes(min)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn comment(id: u64, age_min: i64, first_line: &str) -> String {
    format!(
        r#"{{"id":{id},"updated_at":"{}","body":"{first_line}",{AUTHOR}}}"#,
        stamp_ago(age_min)
    ) + "\n"
}

fn pushes(dir: &Path, list: &[(i64, &str)]) {
    let ndjson: String = list
        .iter()
        .map(|(age, login)| {
            format!(r#"{{"timestamp":"{}","login":"{login}"}}"#, stamp_ago(*age)) + "\n"
        })
        .collect();
    fs::write(dir.join("pushes"), ndjson).unwrap();
}

fn posted(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("posted"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn activity_reads(dir: &Path) -> String {
    fs::read_to_string(dir.join("activity")).unwrap_or_default()
}

#[test]
fn a_doctor_stands_down_on_another_roles_live_pr_lease_without_writing() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    pushes(dir.path(), &[]);
    let judge = who(Role::Judge, "sweep-j");
    fs::write(dir.path().join("pr_rows"), comment(100, 2, &judge.lease_marker())).unwrap();
    assert_eq!(ctx(dir.path(), who(Role::Doctor, "sweep-me")).claim(), EXIT_ACTIVE);
    assert!(posted(dir.path()).is_empty(), "nothing written");
}

#[test]
fn a_doctor_stands_down_on_a_live_lease_on_the_closing_issue_but_not_its_own() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    pushes(dir.path(), &[]);
    // An attended session's builder lease on the issue the PR closes.
    fs::write(
        dir.path().join("issue_rows"),
        comment(40, 3, &issue_marker("host-op", "attended")),
    )
    .unwrap();
    assert_eq!(ctx(dir.path(), who(Role::Doctor, "sweep-me")).claim(), EXIT_ACTIVE);
    assert!(posted(dir.path()).is_empty());

    // This claimant's own issue lease (an in-session sweep) is not activity.
    fs::write(dir.path().join("issue_rows"), comment(40, 3, &issue_marker(HOST, "sweep-me")))
        .unwrap();
    let me = who(Role::Doctor, "sweep-me");
    assert_eq!(ctx(dir.path(), me.clone()).claim(), EXIT_HELD);
    assert_eq!(posted(dir.path()), vec![me.lease_marker()]);
}

#[test]
fn a_doctor_stands_down_on_a_live_issue_lease_on_the_pr_itself() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    pushes(dir.path(), &[]);
    fs::write(dir.path().join("body"), "no closing keyword").unwrap();
    fs::write(dir.path().join("pr_rows"), comment(40, 1, &issue_marker("host-op", "attended")))
        .unwrap();
    assert_eq!(ctx(dir.path(), who(Role::Doctor, "sweep-me")).claim(), EXIT_ACTIVE);
}

#[test]
fn a_doctor_stands_down_on_a_recent_push_by_another_login() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    pushes(dir.path(), &[(4, "rjwalters"), (30, SELF_LOGIN)]);
    assert_eq!(ctx(dir.path(), who(Role::Doctor, "sweep-me")).claim(), EXIT_ACTIVE);
    assert!(posted(dir.path()).is_empty());
    let read = activity_reads(dir.path());
    assert!(read.contains("ref=refs/heads/feature/issue-12"), "{read}");
    assert!(read.contains("activity_type=push"), "{read}");
}

#[test]
fn our_own_push_or_an_old_one_does_not_hold_the_pr() {
    for list in [vec![(2, SELF_LOGIN)], vec![(20, "rjwalters")]] {
        let dir = TempDir::new().unwrap();
        fake_gh(dir.path());
        pushes(dir.path(), &list);
        assert_eq!(ctx(dir.path(), who(Role::Doctor, "sweep-me")).claim(), EXIT_HELD, "{list:?}");
    }
}

#[test]
fn unreadable_extra_evidence_fails_open() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    // No `pushes` file: the activity read fails; the claim proceeds.
    assert_eq!(ctx(dir.path(), who(Role::Doctor, "sweep-me")).claim(), EXIT_HELD);
    assert!(!activity_reads(dir.path()).is_empty(), "the read was attempted");
}

#[test]
fn a_fork_head_is_not_checked() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    pushes(dir.path(), &[(1, "someone")]);
    fs::write(
        dir.path().join("head"),
        r#"{"branch":"main","head_repo":"someone/widgets","base_repo":"acme/widgets"}"#,
    )
    .unwrap();
    assert_eq!(ctx(dir.path(), who(Role::Doctor, "sweep-me")).claim(), EXIT_HELD);
    assert!(activity_reads(dir.path()).is_empty(), "no activity read for a fork");
}

#[test]
fn the_guard_is_the_doctors_only() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    pushes(dir.path(), &[(1, "rjwalters")]);
    fs::write(dir.path().join("issue_rows"), comment(40, 1, &issue_marker("host-op", "a")))
        .unwrap();
    assert_eq!(ctx(dir.path(), who(Role::Judge, "sweep-j")).claim(), EXIT_HELD);
    assert!(activity_reads(dir.path()).is_empty());
}

#[test]
fn a_doctor_re_attaching_to_its_own_lease_is_not_re_checked() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    let me = who(Role::Doctor, "sweep-me");
    // The step engine pre-took the lease; a push lands; the session's own
    // claim re-attaches rather than standing down on activity it is part of.
    fs::write(dir.path().join("pr_rows"), comment(100, 1, &me.lease_marker())).unwrap();
    pushes(dir.path(), &[(1, "rjwalters")]);
    assert_eq!(ctx(dir.path(), me).claim(), EXIT_HELD);
    assert!(posted(dir.path()).is_empty());
}
