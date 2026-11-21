//! #13942: the fleet token-probe snapshot — build, sidecar round-trip,
//! apply, and the newest-probe evidence merge the bad_tokens readers use.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use chrono::Duration;
use std::fs;

fn account(name: &str, status: &str) -> TokenProbeAccount {
    TokenProbeAccount {
        name: name.into(),
        status: status.into(),
        util_5h: None,
        limit_reset: None,
    }
}

fn snapshot(
    probed_at: chrono::DateTime<Utc>,
    accounts: Vec<TokenProbeAccount>,
) -> TokenProbeSnapshot {
    TokenProbeSnapshot {
        probed_at,
        accounts,
    }
}

#[test]
fn snapshot_builds_from_a_ranking_file_with_mtime_as_the_probe_instant() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(snapshot_from_tokens_dir(dir.path()), None, "no .ranking");

    fs::write(
        dir.path().join(".ranking"),
        "agent-1|available|0.42\nagent-2|exhausted||2026-11-20T03:00:00Z\n# a comment line\n",
    )
    .unwrap();
    let built = snapshot_from_tokens_dir(dir.path()).unwrap();
    assert_eq!(built.accounts.len(), 2, "comments and unparseable lines are skipped");
    assert_eq!(built.accounts[0].name, "agent-1");
    assert_eq!(built.accounts[0].status, "available");
    assert_eq!(built.accounts[0].util_5h, Some(0.42));
    assert_eq!(built.accounts[0].limit_reset, None);
    assert_eq!(built.accounts[1].limit_reset.as_deref(), Some("2026-11-20T03:00:00Z"));
    // The probe instant is the file's mtime, the freshness fact the
    // evidence readers already used pre-#13942.
    let mtime: DateTime<Utc> = std::fs::metadata(dir.path().join(".ranking"))
        .unwrap()
        .modified()
        .unwrap()
        .into();
    assert!((built.probed_at - mtime).num_seconds().abs() <= 2);
}

#[test]
fn sidecar_round_trips_and_degrades_to_none() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(read_sidecar(dir.path()), None, "absent file");

    let snap = snapshot(
        "2026-11-18T12:00:00Z".parse().unwrap(),
        vec![TokenProbeAccount {
            name: "agent-1".into(),
            status: "exhausted".into(),
            util_5h: None,
            limit_reset: Some("2026-11-20T03:00:00Z".into()),
        }],
    );
    write_sidecar(dir.path(), &snap).unwrap();
    assert_eq!(read_sidecar(dir.path()), Some(snap));

    let path = dir.path().join(SIDECAR_FILE_NAME);
    fs::write(&path, "not json").unwrap();
    assert_eq!(read_sidecar(dir.path()), None, "invalid JSON");
    // A sidecar from a future schema version is unknown, not guessed at.
    fs::write(&path, r#"{"schema":2,"probed_at":"2026-11-18T12:00:00Z","accounts":[]}"#).unwrap();
    assert_eq!(read_sidecar(dir.path()), None, "unknown schema");
}

#[test]
fn apply_stores_newer_snapshots_and_never_regresses() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let older = snapshot(now - Duration::hours(2), vec![account("agent-1", "available")]);
    let newer = snapshot(now, vec![account("agent-1", "exhausted")]);

    // Nothing on disk: the first snapshot is applied however old it is —
    // a fresh worker's first evidence must not wait for the next cycle.
    assert_eq!(apply_snapshot(dir.path(), &older, now), ApplyOutcome::Applied);
    assert_eq!(read_sidecar(dir.path()).as_ref(), Some(&older));

    // Same cycle again: nothing to write.
    assert_eq!(apply_snapshot(dir.path(), &older, now), ApplyOutcome::Unchanged);
    // An older answer (a stale heartbeat racing a newer one) never regresses.
    let stale = snapshot(now - Duration::hours(3), vec![account("agent-1", "available")]);
    assert_eq!(apply_snapshot(dir.path(), &stale, now), ApplyOutcome::Stale);
    assert_eq!(read_sidecar(dir.path()).as_ref(), Some(&older), "the older probe did not land");

    assert_eq!(apply_snapshot(dir.path(), &newer, now), ApplyOutcome::Applied);
    assert_eq!(read_sidecar(dir.path()), Some(newer));
}

/// The applied-snapshot-age gauge rides every apply — including the
/// `Unchanged` path, where it is the steady-state drift signal.
#[test]
fn apply_emits_the_age_gauge() {
    let dir = tempfile::tempdir().unwrap();
    let now = Utc::now();
    let snap = snapshot(now - Duration::minutes(5), vec![account("agent-1", "available")]);
    let (_, captured) =
        crate::observability::ops::capture::capture(|| apply_snapshot(dir.path(), &snap, now));
    let age: Vec<_> = captured
        .metrics
        .iter()
        .filter(|p| p.name == crate::telemetry::ops::MetricName::TokensFleetProbeAge)
        .collect();
    assert_eq!(age.len(), 1);
    assert_eq!(age[0].value, crate::telemetry::ops::MetricValue::Int(300));
}

#[test]
fn evidence_prefers_the_newest_probe_local_first() {
    let dir = tempfile::tempdir().unwrap();

    // Only a fleet sidecar: a fresh worker's whole evidence picture.
    let snap = snapshot(
        Utc::now() - Duration::minutes(1),
        vec![TokenProbeAccount {
            name: "agent-1".into(),
            status: "exhausted".into(),
            util_5h: None,
            limit_reset: Some("2026-11-20T03:00:00Z".into()),
        }],
    );
    write_sidecar(dir.path(), &snap).unwrap();
    let row = evidence_row(dir.path(), "agent-1").unwrap();
    assert_eq!(row.status, "exhausted");
    assert_eq!(row.limit_reset.as_deref(), Some("2026-11-20T03:00:00Z"));
    assert_eq!(row.probed_at, snap.probed_at.timestamp());
    assert_eq!(evidence_row(dir.path(), "agent-2"), None, "no row either source");

    // A local `.ranking` written now (mtime = now) outranks a sidecar whose
    // recorded probe is older: today's local-first behaviour, unchanged.
    fs::write(dir.path().join(".ranking"), "agent-1|available|0.10\n").unwrap();
    let local = evidence_row(dir.path(), "agent-1").unwrap();
    assert_eq!(local.status, "available", "the newer local probe wins");
    assert_eq!(local.util_5h, Some(0.10));

    // A sidecar recorded strictly after the local `.ranking` mtime takes
    // over: the captain probed after this host's last local probe.
    let newer_snapshot =
        snapshot(Utc::now() + Duration::minutes(2), vec![account("agent-1", "blocked")]);
    write_sidecar(dir.path(), &newer_snapshot).unwrap();
    let fleet = evidence_row(dir.path(), "agent-1").unwrap();
    assert_eq!(fleet.status, "blocked", "the newer fleet probe wins");
}

/// The captain's per-cycle record: one point per account, labelled with the
/// verdict, for every distinct probe instant.
#[test]
fn published_cycle_points_cover_every_account_once() {
    let snap = snapshot(
        Utc::now(),
        vec![
            account("agent-1", "available"),
            TokenProbeAccount {
                name: "agent-2".into(),
                status: "exhausted".into(),
                util_5h: Some(1.0),
                limit_reset: None,
            },
        ],
    );
    let points = published_cycle_points(&snap);
    assert_eq!(points.len(), 2);
    assert!(points
        .iter()
        .all(|p| p.name == crate::telemetry::ops::MetricName::TokensFleetProbeAccounts));
    assert_eq!(points[0].labels["account"], "agent-1");
    assert_eq!(points[0].labels["state"], "available");
    assert_eq!(points[1].labels["state"], "exhausted");
}
