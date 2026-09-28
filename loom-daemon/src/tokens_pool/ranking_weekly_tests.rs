use super::*;
use crate::tokens_pool::check::AccountResult;

fn account(name: &str, status: &str, s7d: Option<f64>) -> AccountResult {
    let mut result = AccountResult::new(name, status);
    result.s7d_utilization = s7d;
    result
}

fn report(accounts: Vec<AccountResult>) -> ProbeReport {
    ProbeReport {
        ranked_at: "2026-09-27T00:00:00Z".into(),
        accounts,
    }
}

#[test]
fn round_trips_known_weekly_values_and_omits_unknown_ones() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".ranking"), "a|available|0.10\nb|exhausted\n").unwrap();
    let probe = report(vec![
        account("a", "available", Some(0.37)),
        account("b", "exhausted", Some(1.0)),
        account("c", "error", None),
        account("d", "unsupported", Some(0.5)),
    ]);
    write_weekly_utilization_sidecar(&probe, dir.path(), Utc::now()).unwrap();

    let read = read_weekly_utilization_sidecar(dir.path());
    assert_eq!(read.get("a"), Some(&0.37));
    assert_eq!(read.get("b"), Some(&1.0));
    assert_eq!(read.get("c"), None, "an unknown 7d reading must stay absent, not 0");
    assert_eq!(read.get("d"), None, "unsupported rows are omitted like .ranking omits them");
}

#[test]
fn a_measured_zero_is_kept_as_zero() {
    let dir = tempfile::tempdir().unwrap();
    write_weekly_utilization_sidecar(
        &report(vec![account("a", "available", Some(0.0))]),
        dir.path(),
        Utc::now(),
    )
    .unwrap();
    assert_eq!(read_weekly_utilization_sidecar(dir.path()).get("a"), Some(&0.0));
}

#[test]
fn missing_or_malformed_sidecar_degrades_to_empty() {
    let dir = tempfile::tempdir().unwrap();
    assert!(read_weekly_utilization_sidecar(dir.path()).is_empty(), "absent file");

    let path = dir.path().join(SIDECAR_FILE_NAME);
    std::fs::write(&path, "not json").unwrap();
    assert!(read_weekly_utilization_sidecar(dir.path()).is_empty(), "invalid JSON");

    std::fs::write(
        &path,
        r#"{"schema":2,"written_at":"2026-09-27T00:00:00Z","accounts":{"a":0.5}}"#,
    )
    .unwrap();
    assert!(read_weekly_utilization_sidecar(dir.path()).is_empty(), "unknown schema");

    std::fs::write(&path, r#"{"schema":1,"written_at":"garbage","accounts":{"a":0.5}}"#).unwrap();
    assert!(read_weekly_utilization_sidecar(dir.path()).is_empty(), "unparseable written_at");
}

#[test]
fn a_bad_value_drops_only_that_account() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    std::fs::write(
        dir.path().join(SIDECAR_FILE_NAME),
        format!(r#"{{"schema":1,"written_at":"{now}","accounts":{{"a":0.4,"b":"high","c":-1.0,"d":null}}}}"#),
    )
    .unwrap();
    let read = read_weekly_utilization_sidecar(dir.path());
    assert_eq!(read.len(), 1);
    assert_eq!(read.get("a"), Some(&0.4));
}

#[test]
fn a_ranking_rewritten_after_the_sidecar_makes_it_stale() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".ranking"), "a|available|0.10\n").unwrap();
    // A sidecar stamped well before the `.ranking` beside it describes an
    // older probe: some later writer replaced `.ranking` without it.
    let old = Utc::now() - chrono::Duration::hours(2);
    write_weekly_utilization_sidecar(
        &report(vec![account("a", "available", Some(0.9))]),
        dir.path(),
        old,
    )
    .unwrap();
    assert!(read_weekly_utilization_sidecar(dir.path()).is_empty());
}
