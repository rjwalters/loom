//! Tests for the daemon-hold park record (#10161).
//!
//! Two layers. A recording fake [`BoundedParkForge`] pins [`record_hold`]'s own
//! contract (read, then write; refusal vs timeout). A fake `gh` drives both real
//! writers, [`SweepRegistry::apply_quarantine_label`] and the PR-less hold, and
//! pins the order on the wire: the body record before `loom:blocked`.

#![allow(clippy::unwrap_used)]

use super::*;
use crate::park_record::BlockerRef;
use crate::sweep_registry::reaper::REAP_GH_TIMEOUT_ENV;
use crate::sweep_registry::test_support::{fake_gh_graphql_arm, fake_gh_timeline_rest_arm};
use crate::sweep_registry::{PrlessRetryConfig, SweepRegistryConfig};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::tempdir;

const AT: &str = "2026-10-06T12:00:00Z";

// ---------------------------------------------------------------------------
// The record contract
// ---------------------------------------------------------------------------

#[test]
fn both_daemon_hold_records_parse_back_as_daemon_holds() {
    for reason in [QUARANTINE_HOLD_REASON, PRLESS_HOLD_REASON] {
        let line = render_hold_record(reason, AT);
        let records = park_record::parse(&line);
        assert_eq!(records.len(), 1, "{line}");
        let r = &records[0];
        assert!(is_daemon_hold(r), "{r:?}");
        assert_eq!(r.blocker, None);
        assert_eq!(r.by.as_deref(), Some(DAEMON_HOLD_BY));
        assert_eq!(r.at.as_deref(), Some(AT));
        assert_eq!(r.reason.as_deref(), Some(reason));
        assert!(park_record::blockers(&line).is_empty(), "a daemon hold names no blocker");
    }
}

#[test]
fn is_daemon_hold_rejects_role_parks_and_other_reasons() {
    let hold = ParkRecord {
        blocker: None,
        by: Some(DAEMON_HOLD_BY.to_string()),
        at: Some(AT.to_string()),
        reason: Some(PRLESS_HOLD_REASON.to_string()),
    };
    assert!(is_daemon_hold(&hold));
    let numbered = ParkRecord {
        blocker: Some(BlockerRef::local(42)),
        ..hold.clone()
    };
    assert!(!is_daemon_hold(&numbered), "a numbered blocker is a dependency park");
    let by_role = ParkRecord {
        by: Some("curator".to_string()),
        ..hold.clone()
    };
    assert!(!is_daemon_hold(&by_role));
    let operator = ParkRecord {
        reason: Some("operator".to_string()),
        ..hold.clone()
    };
    assert!(!is_daemon_hold(&operator));
    let no_reason = ParkRecord {
        reason: None,
        ..hold
    };
    assert!(!is_daemon_hold(&no_reason));
}

#[test]
fn compose_appends_after_the_body_with_one_blank_line() {
    let out = compose_hold_body("Fix the thing.\n\n", QUARANTINE_HOLD_REASON, AT);
    assert_eq!(
        out,
        format!("Fix the thing.\n\n{}\n", render_hold_record(QUARANTINE_HOLD_REASON, AT))
    );
    let empty = compose_hold_body("", PRLESS_HOLD_REASON, AT);
    assert_eq!(empty, format!("{}\n", render_hold_record(PRLESS_HOLD_REASON, AT)));
}

#[test]
fn reapplying_a_hold_replaces_its_own_record_and_keeps_everyone_elses() {
    let role_park =
        park_record::render_park(&[BlockerRef::local(8860)], Some("curator"), Some(AT), None);
    let other_hold = render_hold_record(QUARANTINE_HOLD_REASON, AT);
    let old_hold = render_hold_record(PRLESS_HOLD_REASON, "2026-10-01T00:00:00Z");
    let body = format!("Body.\r\n\r\n{role_park}\r\n{other_hold}\r\n{old_hold}\r\n");

    let out = compose_hold_body(&body, PRLESS_HOLD_REASON, AT);

    let records = park_record::parse(&out);
    let prless: Vec<_> = records
        .iter()
        .filter(|r| r.reason.as_deref() == Some(PRLESS_HOLD_REASON))
        .collect();
    assert_eq!(prless.len(), 1, "one current pr-less record, not two: {out}");
    assert_eq!(prless[0].at.as_deref(), Some(AT), "`at=` dates the current park");
    assert_eq!(
        park_record::blockers(&out),
        vec![BlockerRef::local(8860)],
        "the role's park is kept"
    );
    assert!(
        records
            .iter()
            .any(|r| r.reason.as_deref() == Some(QUARANTINE_HOLD_REASON)),
        "the other daemon writer's record is kept: {out}"
    );
    assert!(out.starts_with("Body.\r\n\r\n"), "untouched lines keep their CRLF: {out:?}");
}

/// A recording fake: every call is appended to `calls`.
#[derive(Default)]
struct FakeForge {
    body: String,
    calls: Vec<String>,
    view_err: Option<String>,
    set_body_err: Option<String>,
    timed_out: bool,
}

impl ParkForge for FakeForge {
    fn view(&mut self, number: u64) -> Result<IssueState, String> {
        self.calls.push(format!("view {number}"));
        match &self.view_err {
            Some(e) => Err(e.clone()),
            None => Ok(IssueState {
                body: self.body.clone(),
                labels: vec!["loom:issue".to_string()],
            }),
        }
    }
    fn state(&mut self, repo: Option<&str>, number: u64) -> Result<String, String> {
        self.calls.push(format!("state {repo:?} {number}"));
        Ok("open".to_string())
    }
    fn set_body(&mut self, number: u64, body: &str) -> Result<(), String> {
        self.calls.push(format!("set_body {number}"));
        if let Some(e) = &self.set_body_err {
            return Err(e.clone());
        }
        self.body = body.to_string();
        Ok(())
    }
    fn add_labels(&mut self, number: u64, labels: &[String]) -> Result<(), String> {
        self.calls
            .push(format!("add_labels {number} {}", labels.join(",")));
        Ok(())
    }
    fn remove_label(&mut self, number: u64, label: &str) -> Result<(), String> {
        self.calls.push(format!("remove_label {number} {label}"));
        Ok(())
    }
}

impl BoundedParkForge for FakeForge {
    fn timed_out(&self) -> bool {
        self.timed_out
    }
}

#[test]
fn record_hold_reads_then_writes_the_body_and_touches_no_label() {
    let mut forge = FakeForge {
        body: "Original.".to_string(),
        ..FakeForge::default()
    };
    let out = record_hold(&mut forge, 7, PRLESS_HOLD_REASON, AT);
    assert_eq!(out, HoldRecord::Written);
    assert_eq!(forge.calls, vec!["view 7", "set_body 7"]);
    assert!(forge.body.starts_with("Original.\n\n"));
    assert!(park_record::parse(&forge.body).iter().any(is_daemon_hold));
}

#[test]
fn record_hold_writes_nothing_when_the_body_already_carries_this_record() {
    let mut forge = FakeForge {
        body: compose_hold_body("Original.", PRLESS_HOLD_REASON, AT),
        ..FakeForge::default()
    };
    assert_eq!(record_hold(&mut forge, 7, PRLESS_HOLD_REASON, AT), HoldRecord::Written);
    assert_eq!(forge.calls, vec!["view 7"], "an identical body is not rewritten");
}

#[test]
fn a_refused_read_or_write_is_rejected_not_timed_out() {
    let mut forge = FakeForge {
        view_err: Some("HTTP 404".to_string()),
        ..FakeForge::default()
    };
    let out = record_hold(&mut forge, 7, QUARANTINE_HOLD_REASON, AT);
    assert!(matches!(out, HoldRecord::Rejected(ref e) if e.contains("HTTP 404")), "{out:?}");
    assert_eq!(forge.calls, vec!["view 7"], "no write after a failed read");

    let mut forge = FakeForge {
        set_body_err: Some("HTTP 403".to_string()),
        ..FakeForge::default()
    };
    let out = record_hold(&mut forge, 7, QUARANTINE_HOLD_REASON, AT);
    assert!(matches!(out, HoldRecord::Rejected(ref e) if e.contains("HTTP 403")), "{out:?}");
}

#[test]
fn a_timed_out_call_is_reported_as_a_timeout() {
    let mut forge = FakeForge {
        view_err: Some("killed".to_string()),
        timed_out: true,
        ..FakeForge::default()
    };
    let out = record_hold(&mut forge, 7, QUARANTINE_HOLD_REASON, AT);
    assert!(matches!(out, HoldRecord::TimedOut(_)), "{out:?}");
}

// ---------------------------------------------------------------------------
// Both writers, against a fake `gh`
// ---------------------------------------------------------------------------

/// Restores [`REAP_GH_TIMEOUT_ENV`] on drop.
struct TimeoutEnv;
impl TimeoutEnv {
    fn set(secs: &str) -> Self {
        std::env::set_var(REAP_GH_TIMEOUT_ENV, secs);
        Self
    }
}
impl Drop for TimeoutEnv {
    fn drop(&mut self) {
        std::env::remove_var(REAP_GH_TIMEOUT_ENV);
    }
}

/// How the fake `gh` answers the body read and the body write.
struct Script<'a> {
    /// What the body read prints (post-`--jq` shape).
    view: &'a str,
    /// Seconds the body read sleeps first (a wedged `gh`).
    view_sleep: u32,
    /// Exit code of the body `PATCH`.
    patch_exit: i32,
}

/// A registry with forge writes enabled, driven by a fake `gh` that logs every
/// argv to `gh.log` and every `PATCH` payload to `payloads.log`.
fn registry(ws: &Path, s: &Script) -> (SweepRegistry, PathBuf, PathBuf) {
    let log = ws.join("gh.log");
    let payloads = ws.join("payloads.log");
    let fake = ws.join("fake-gh-park-hold.sh");
    let script = format!(
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >> \"{log}\"\n\
         if [[ \"$1\" == \"api\" && \"$2\" == \"-X\" && \"$3\" == \"PATCH\" ]]; then\n\
         cat \"$6\" >> \"{payloads}\"; printf '\\n' >> \"{payloads}\"\n\
         if [[ {patch} -ne 0 ]]; then printf 'HTTP 403: body write refused\\n' >&2; fi\n\
         exit {patch}\n\
         fi\n\
         if [[ \"$1\" == \"api\" && \"$3\" == \"--jq\" && \"$4\" == \"{{number\"* ]]; then\n\
         sleep {sleep}\n\
         printf '%s\\n' '{view}'\n\
         exit 0\n\
         fi\n\
         {timeline}\
         {gql}\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
         printf '%s\\n' '{{\"state\":\"open\",\"is_pr\":false}}'\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
         printf 'rjwalters/loom\\n'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        log = log.display(),
        payloads = payloads.display(),
        patch = s.patch_exit,
        sleep = s.view_sleep,
        view = s.view,
        timeline = fake_gh_timeline_rest_arm("", 0),
        gql = fake_gh_graphql_arm("", 0),
    );
    std::fs::write(&fake, &script).unwrap();
    let mut perms = std::fs::metadata(&fake).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&fake, perms).unwrap();
    if let Ok(f) = std::fs::File::open(&fake) {
        let _ = f.sync_all();
    }
    let mut config = SweepRegistryConfig::new(ws.to_path_buf());
    config.gh_bin = Some(fake);
    config.skip_label_flip = false;
    config.journal_path = Some(ws.join("test-sweeps-journal.json"));
    (SweepRegistry::new(config), log, payloads)
}

/// The argv lines (first line of each call), in call order.
fn calls(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| ["api ", "issue ", "repo "].iter().any(|p| l.starts_with(p)))
        .map(str::to_string)
        .collect()
}

fn position(calls: &[String], prefix: &str) -> Option<usize> {
    calls.iter().position(|c| c.starts_with(prefix))
}

/// The bodies the fake `gh` was asked to PATCH.
fn patched_bodies(payloads: &Path) -> Vec<String> {
    std::fs::read_to_string(payloads)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            v["body"].as_str().unwrap().to_string()
        })
        .collect()
}

fn view_answer(n: u32) -> String {
    format!(r#"{{"number":{n},"body":"Original body.","labels":["loom:issue"]}}"#)
}

#[test]
#[serial]
fn quarantine_writes_the_record_before_the_label_and_keeps_its_comment() {
    std::env::remove_var("LOOM_REPO");
    let dir = tempdir().unwrap();
    let view = view_answer(9501);
    let (reg, log, payloads) = registry(
        dir.path(),
        &Script {
            view: &view,
            view_sleep: 0,
            patch_exit: 0,
        },
    );

    reg.apply_quarantine_label(9501, 3);

    let calls = calls(&log);
    let patch = position(&calls, "api -X PATCH repos/rjwalters/loom/issues/9501 --input ")
        .unwrap_or_else(|| panic!("no body write: {calls:?}"));
    let label =
        position(&calls, "issue edit 9501 --add-label loom:blocked --remove-label loom:issue")
            .unwrap_or_else(|| panic!("no label edit: {calls:?}"));
    let comment = position(&calls, "issue comment 9501 --body ")
        .unwrap_or_else(|| panic!("no comment: {calls:?}"));
    assert!(patch < label, "the record lands before `loom:blocked`: {calls:?}");
    assert!(label < comment, "the comment is unchanged and still follows: {calls:?}");

    let bodies = patched_bodies(&payloads);
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    assert!(bodies[0].starts_with("Original body.\n\n"), "the body is kept: {}", bodies[0]);
    let records = park_record::parse(&bodies[0]);
    assert_eq!(records.len(), 1);
    assert!(is_daemon_hold(&records[0]));
    assert_eq!(records[0].reason.as_deref(), Some(QUARANTINE_HOLD_REASON));
    assert!(records[0].at.as_deref().is_some_and(|a| a.ends_with('Z')), "RFC 3339 `at=`");
}

#[test]
#[serial]
fn quarantine_still_labels_when_the_body_write_is_refused() {
    std::env::remove_var("LOOM_REPO");
    let dir = tempdir().unwrap();
    let view = view_answer(9501);
    let (reg, log, _) = registry(
        dir.path(),
        &Script {
            view: &view,
            view_sleep: 0,
            patch_exit: 1,
        },
    );

    reg.apply_quarantine_label(9501, 3);

    let calls = calls(&log);
    assert!(position(&calls, "api -X PATCH").is_some(), "{calls:?}");
    assert!(
        position(&calls, "issue edit 9501 --add-label loom:blocked").is_some(),
        "a refused record falls back to the label (documented): {calls:?}"
    );
}

#[test]
#[serial]
fn an_answer_that_is_not_this_issue_is_never_written_back() {
    std::env::remove_var("LOOM_REPO");
    for view in [
        r#"{"state":"open","is_pr":false}"#.to_string(),
        view_answer(1234),
    ] {
        let dir = tempdir().unwrap();
        let (reg, log, payloads) = registry(
            dir.path(),
            &Script {
                view: &view,
                view_sleep: 0,
                patch_exit: 0,
            },
        );

        reg.apply_quarantine_label(9501, 3);

        assert!(
            patched_bodies(&payloads).is_empty(),
            "a body built from a foreign answer would erase the real one ({view})"
        );
        assert!(position(&calls(&log), "issue edit 9501 --add-label loom:blocked").is_some());
    }
}

#[test]
#[serial]
fn a_wedged_gh_costs_quarantine_one_timeout_and_no_label() {
    std::env::remove_var("LOOM_REPO");
    let _timeout = TimeoutEnv::set("1");
    let dir = tempdir().unwrap();
    let view = view_answer(9501);
    let (reg, log, _) = registry(
        dir.path(),
        &Script {
            view: &view,
            view_sleep: 4,
            patch_exit: 0,
        },
    );

    reg.apply_quarantine_label(9501, 3);

    let calls = calls(&log);
    assert!(position(&calls, "api -X PATCH").is_none(), "{calls:?}");
    assert!(position(&calls, "issue edit").is_none(), "no second timeout: {calls:?}");
    assert!(position(&calls, "issue comment").is_none(), "{calls:?}");
}

fn hold_at_threshold(reg: &mut SweepRegistry, issue: u32) {
    reg.set_prless_retry_config(PrlessRetryConfig {
        threshold: 2,
        ..PrlessRetryConfig::default()
    });
    for _ in 0..2 {
        reg.record_prless_release(issue, "builder crashed without opening a PR");
    }
}

#[test]
#[serial]
fn the_prless_hold_writes_the_record_before_the_label() {
    std::env::remove_var("LOOM_REPO");
    let dir = tempdir().unwrap();
    let view = view_answer(7893);
    let (mut reg, log, payloads) = registry(
        dir.path(),
        &Script {
            view: &view,
            view_sleep: 0,
            patch_exit: 0,
        },
    );

    hold_at_threshold(&mut reg, 7893);

    assert!(reg.prless_retry_held(7893));
    let calls = calls(&log);
    let patch = position(&calls, "api -X PATCH repos/rjwalters/loom/issues/7893 --input ")
        .unwrap_or_else(|| panic!("no body write: {calls:?}"));
    let label =
        position(&calls, "issue edit 7893 --add-label loom:blocked --remove-label loom:issue")
            .unwrap_or_else(|| panic!("no label edit: {calls:?}"));
    assert!(patch < label, "the record lands before `loom:blocked`: {calls:?}");
    let bodies = patched_bodies(&payloads);
    assert_eq!(bodies.len(), 1, "{bodies:?}");
    let records = park_record::parse(&bodies[0]);
    assert!(
        records
            .iter()
            .any(|r| is_daemon_hold(r) && r.reason.as_deref() == Some(PRLESS_HOLD_REASON)),
        "{records:?}"
    );
}

#[test]
#[serial]
fn the_prless_hold_still_parks_when_the_body_write_is_refused() {
    std::env::remove_var("LOOM_REPO");
    let dir = tempdir().unwrap();
    let view = view_answer(7893);
    let (mut reg, log, _) = registry(
        dir.path(),
        &Script {
            view: &view,
            view_sleep: 0,
            patch_exit: 1,
        },
    );

    hold_at_threshold(&mut reg, 7893);

    assert!(
        reg.prless_retry_held(7893),
        "the label is the deliverable (#9239): a refused record does not cancel the park"
    );
    assert!(position(&calls(&log), "issue edit 7893 --add-label loom:blocked").is_some());
}

#[test]
#[serial]
fn a_wedged_gh_fails_the_prless_hold_closed_without_a_label_edit() {
    std::env::remove_var("LOOM_REPO");
    let _timeout = TimeoutEnv::set("1");
    let dir = tempdir().unwrap();
    let view = view_answer(7893);
    let (mut reg, log, _) = registry(
        dir.path(),
        &Script {
            view: &view,
            view_sleep: 4,
            patch_exit: 0,
        },
    );

    hold_at_threshold(&mut reg, 7893);

    assert!(
        !reg.prless_retry_held(7893),
        "an unapplied park is never recorded as a hold (#9239)"
    );
    let calls = calls(&log);
    assert!(position(&calls, "issue edit").is_none(), "no second timeout: {calls:?}");
}
