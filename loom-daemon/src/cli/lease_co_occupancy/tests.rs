//! Tests for `loom-daemon lease co-occupancy` (rjwalters/kicad-tools#5783).
//!
//! The decision (`decide` / `live_pairs`) is pure and exercised directly; the
//! forge read (`read_rows`) is driven against FAKE `gh` scripts so the
//! fail-open paths — a failing read, a hung read — are observed end to end.

use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::time::Instant;
use tempfile::TempDir;

use loom_daemon::forge_identity::{FleetLogins, Roster};

const ISSUE: u64 = 5781;

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-28T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn ttl() -> chrono::Duration {
    chrono::Duration::minutes(15)
}

fn lease(host: &str, sweep: &str, age_min: i64) -> LeaseRow {
    LeaseRow {
        updated_at: now() - chrono::Duration::minutes(age_min),
        body: format!("<!-- loom:lease host={host} sweep={sweep} -->\nprose"),
    }
}

fn yield_rec(host: &str, sweep: &str, age_min: i64) -> LeaseRow {
    LeaseRow {
        updated_at: now() - chrono::Duration::minutes(age_min),
        body: format!(
            "<!-- loom:lease-yield host={host} sweep={sweep} earliest_host=h0 \
             earliest_sweep=s0 -->\nprose"
        ),
    }
}

fn run(rows: Option<&[LeaseRow]>, allow: bool, json: bool) -> Verdict {
    decide(ISSUE, rows, now(), ttl(), allow, json)
}

#[test]
fn no_lease_comments_proceeds_silently() {
    let v = run(Some(&[]), false, false);
    assert_eq!(v.exit_code, 0);
    assert!(v.stderr.is_empty() && v.stdout.is_none());
}

#[test]
fn exactly_one_fresh_lease_proceeds() {
    let v = run(Some(&[lease("h1", "sweep-a", 1)]), false, false);
    assert_eq!(v.exit_code, 0, "a single sweep resuming its own worktree is legitimate");
}

#[test]
fn the_same_pair_renewed_twice_is_one_live_lease_not_two() {
    let rows = [lease("h1", "sweep-a", 1), lease("h1", "sweep-a", 3)];
    assert_eq!(run(Some(&rows), false, false).exit_code, 0);
}

#[test]
fn two_distinct_fresh_leases_refuse_and_name_each_pair() {
    // The kicad-tools#5781 signature: two same-host sweeps, both fresh.
    let rows = [lease("h1", "sweep-a", 2), lease("h1", "sweep-b", 1)];
    let v = run(Some(&rows), false, false);
    assert_eq!(v.exit_code, 1);
    let err = v.stderr.join("\n");
    assert!(err.contains("simultaneously FRESH"), "{err}");
    assert!(err.contains("host=h1 sweep=sweep-a"), "{err}");
    assert!(err.contains("host=h1 sweep=sweep-b"), "{err}");
    assert!(err.contains(OVERRIDE_ENV), "the refusal must name its override: {err}");
    assert!(v.stdout.is_none());
}

#[test]
fn two_fresh_leases_on_different_hosts_also_refuse() {
    let rows = [lease("h1", "sweep-a", 2), lease("h2", "sweep-b", 1)];
    assert_eq!(run(Some(&rows), false, false).exit_code, 1);
}

#[test]
fn one_fresh_and_one_stale_lease_proceeds() {
    let rows = [lease("h1", "sweep-a", 1), lease("h1", "sweep-old", 16)];
    assert_eq!(run(Some(&rows), false, false).exit_code, 0);
}

#[test]
fn a_lease_exactly_at_the_ttl_is_still_fresh() {
    // Matches sweep-lease-publish.sh's `c_age_seconds <= ttl_seconds`.
    let rows = [lease("h1", "sweep-a", 1), lease("h1", "sweep-b", 15)];
    assert_eq!(run(Some(&rows), false, false).exit_code, 1);
}

#[test]
fn a_yielded_lease_is_not_live() {
    let rows = [
        lease("h1", "sweep-a", 2),
        lease("h2", "sweep-b", 1),
        yield_rec("h2", "sweep-b", 0),
    ];
    assert_eq!(run(Some(&rows), false, false).exit_code, 0);
}

#[test]
fn a_yield_by_a_different_pair_does_not_excuse_a_live_lease() {
    let rows = [
        lease("h1", "sweep-a", 2),
        lease("h2", "sweep-b", 1),
        yield_rec("h2", "sweep-other", 0),
    ];
    assert_eq!(run(Some(&rows), false, false).exit_code, 1);
}

#[test]
fn a_failed_read_fails_open() {
    let v = run(None, false, false);
    assert_eq!(v.exit_code, 0);
    assert!(v.stderr.is_empty());
}

#[test]
fn the_override_downgrades_the_refusal_to_a_warning() {
    let rows = [lease("h1", "sweep-a", 2), lease("h1", "sweep-b", 1)];
    let v = run(Some(&rows), true, false);
    assert_eq!(v.exit_code, 0);
    assert!(v.stderr.join("\n").contains("proceeding despite"));
}

#[test]
fn json_mode_puts_the_failure_document_on_stdout() {
    let rows = [lease("h1", "sweep-a", 2), lease("h1", "sweep-b", 1)];
    let v = run(Some(&rows), false, true);
    assert_eq!(v.exit_code, 1);
    let doc: serde_json::Value = serde_json::from_str(v.stdout.as_deref().unwrap()).unwrap();
    assert_eq!(doc["success"], false);
    assert!(doc["error"].as_str().unwrap().contains(OVERRIDE_ENV));
}

#[test]
fn parse_rows_drops_malformed_lines_and_keeps_the_rest() {
    let ndjson = "{\"updated_at\":\"2026-09-28T11:59:00Z\",\"body\":\"<!-- loom:lease host=h sweep=s -->\"}\n\
                  not json\n\
                  {\"updated_at\":\"garbage\",\"body\":\"x\"}\n\
                  {\"body\":\"no timestamp\"}\n";
    let rows = parse_rows(ndjson);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].body.starts_with("<!-- loom:lease host=h"));
}

#[test]
fn yield_marker_parsing_takes_only_the_sweep_token() {
    assert_eq!(
        parse_yield_marker_line(
            "<!-- loom:lease-yield host=h sweep=s earliest_host=h2 earliest_sweep=s2 -->\nx"
        ),
        Some(("h".into(), "s".into()))
    );
    assert_eq!(parse_yield_marker_line("<!-- loom:lease host=h sweep=s -->"), None);
}

fn fake_gh(dir: &Path, script: &str) -> PathBuf {
    let path = dir.join("gh");
    fs::write(&path, format!("#!/usr/bin/env bash\n{script}")).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn read_rows_parses_a_successful_gh_read() {
    let dir = TempDir::new().unwrap();
    let gh = fake_gh(
        dir.path(),
        &format!("printf '%s\\n' '{}'\n", row("<!-- loom:lease host=h sweep=s -->", FLEET_APP)),
    );
    let rows = read_rows(&gh, dir.path(), ISSUE, Duration::from_secs(10), &policy()).unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn read_rows_is_none_when_gh_fails() {
    let dir = TempDir::new().unwrap();
    let gh = fake_gh(dir.path(), "echo 'HTTP 502' >&2\nexit 1\n");
    assert!(read_rows(&gh, dir.path(), ISSUE, Duration::from_secs(10), &policy()).is_none());
}

#[test]
fn read_rows_is_none_when_gh_is_missing() {
    let dir = TempDir::new().unwrap();
    let missing = dir.path().join("no-such-gh");
    assert!(read_rows(&missing, dir.path(), ISSUE, Duration::from_secs(10), &policy()).is_none());
}

#[test]
fn read_rows_gives_up_at_its_deadline() {
    let dir = TempDir::new().unwrap();
    let gh = fake_gh(dir.path(), "sleep 30\n");
    let start = Instant::now();
    assert!(read_rows(&gh, dir.path(), ISSUE, Duration::from_millis(300), &policy()).is_none());
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "a hung read must not stall worktree.sh"
    );
}

// ---- #9631: only trusted authors' lease / lease-yield records count ----

/// The rules for an unconfigured host: the default fleet App family, no self
/// login, no allowlist (what `TrustPolicy::for_root` resolves to with an
/// empty roster).
fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&Roster::default()), None, Vec::new())
}

/// This fleet's default App (trusted on every host).
const FLEET_APP: &str =
    r#""user":{"login":"loom-fleet-dispatch[bot]","type":"Bot"},"author_association":"NONE""#;
/// A repo owner (trusted by association).
const OWNER: &str = r#""user":{"login":"rjwalters","type":"User"},"author_association":"OWNER""#;
/// An outsider with no write access.
const OUTSIDER: &str = r#""user":{"login":"mallory","type":"User"},"author_association":"NONE""#;
/// A look-alike App that is not this fleet's.
const FOREIGN_APP: &str =
    r#""user":{"login":"loom-fleet-dispatch-evil[bot]","type":"Bot"},"author_association":"NONE""#;
/// No author fields at all (e.g. a deleted account's `user: null`).
const NO_AUTHOR: &str = r#""user":null"#;

/// One NDJSON line as the real `--jq` projection emits it.
fn row(body: &str, author: &str) -> String {
    let body = serde_json::to_string(body).unwrap();
    format!(r#"{{"updated_at":"2026-09-28T11:59:00Z","body":{body},{author}}}"#)
}

fn lease_line(host: &str, sweep: &str, author: &str) -> String {
    row(&format!("<!-- loom:lease host={host} sweep={sweep} -->\nprose"), author)
}

fn yield_line(host: &str, sweep: &str, author: &str) -> String {
    row(
        &format!(
            "<!-- loom:lease-yield host={host} sweep={sweep} earliest_host=h0 \
             earliest_sweep=s0 -->\nprose"
        ),
        author,
    )
}

/// `read_rows` over a fake `gh` that prints `lines`, then the verdict.
fn verdict_for(lines: &[String]) -> Verdict {
    let dir = TempDir::new().unwrap();
    let fixture = dir.path().join("rows.ndjson");
    fs::write(&fixture, lines.join("\n") + "\n").unwrap();
    let gh = fake_gh(dir.path(), &format!("cat '{}'\n", fixture.display()));
    let rows = read_rows(&gh, dir.path(), ISSUE, Duration::from_secs(10), &policy());
    assert!(rows.is_some(), "a successful read is evidence, not a failure");
    decide(ISSUE, rows.as_deref(), Utc::now(), chrono::Duration::days(3650), false, false)
}

#[test]
fn trusted_fleet_app_and_owner_leases_are_honoured() {
    let v = verdict_for(&[
        lease_line("h1", "sweep-a", FLEET_APP),
        lease_line("h2", "sweep-b", OWNER),
    ]);
    assert_eq!(v.exit_code, 1, "two trusted fresh leases are a real co-occupancy");
}

#[test]
fn spoofed_leases_from_an_outsider_cannot_force_a_refusal() {
    let v = verdict_for(&[
        lease_line("h1", "sweep-a", FLEET_APP),
        lease_line("evil", "spoof-1", OUTSIDER),
        lease_line("evil", "spoof-2", OUTSIDER),
    ]);
    assert_eq!(v.exit_code, 0, "an outsider's lease is prose: {:?}", v.stderr);
}

#[test]
fn a_look_alike_foreign_app_lease_is_ignored() {
    let v = verdict_for(&[
        lease_line("h1", "sweep-a", FLEET_APP),
        lease_line("h9", "sweep-z", FOREIGN_APP),
    ]);
    assert_eq!(v.exit_code, 0, "{:?}", v.stderr);
}

#[test]
fn a_lease_with_no_author_is_untrusted() {
    let v = verdict_for(&[
        lease_line("h1", "sweep-a", FLEET_APP),
        lease_line("h2", "sweep-b", NO_AUTHOR),
        // Pre-#9631 shape: no author fields at all.
        r#"{"updated_at":"2026-09-28T11:59:00Z","body":"<!-- loom:lease host=h3 sweep=sweep-c -->"}"#
            .to_string(),
    ]);
    assert_eq!(v.exit_code, 0, "{:?}", v.stderr);
}

#[test]
fn a_spoofed_yield_cannot_excuse_a_real_live_lease() {
    let v = verdict_for(&[
        lease_line("h1", "sweep-a", FLEET_APP),
        lease_line("h2", "sweep-b", FLEET_APP),
        yield_line("h2", "sweep-b", OUTSIDER),
    ]);
    assert_eq!(v.exit_code, 1, "an outsider cannot stand a real claimant down");
}

#[test]
fn a_trusted_yield_still_excuses_its_own_lease() {
    let v = verdict_for(&[
        lease_line("h1", "sweep-a", FLEET_APP),
        lease_line("h2", "sweep-b", FLEET_APP),
        yield_line("h2", "sweep-b", FLEET_APP),
    ]);
    assert_eq!(v.exit_code, 0, "{:?}", v.stderr);
}

#[test]
fn every_row_untrusted_is_an_empty_read_not_a_failed_one() {
    let dir = TempDir::new().unwrap();
    let fixture = dir.path().join("rows.ndjson");
    fs::write(&fixture, lease_line("evil", "s", OUTSIDER) + "\n").unwrap();
    let gh = fake_gh(dir.path(), &format!("cat '{}'\n", fixture.display()));
    let rows = read_rows(&gh, dir.path(), ISSUE, Duration::from_secs(10), &policy());
    assert_eq!(rows, Some(Vec::new()));
}

#[test]
fn the_forge_read_projects_the_author() {
    // Without the author in the projection every row would be dropped.
    let dir = TempDir::new().unwrap();
    let args = dir.path().join("args");
    let gh = fake_gh(dir.path(), &format!("printf '%s\\n' \"$@\" > '{}'\n", args.display()));
    read_rows(&gh, dir.path(), ISSUE, Duration::from_secs(10), &policy());
    let recorded = fs::read_to_string(&args).unwrap();
    assert!(recorded.contains(AUTHOR_JQ), "{recorded}");
}
