use std::cell::Cell;
use std::time::Duration;

use super::*;
use crate::fleet_store::fetch::{load, Policy};
use crate::fleet_store::test_support::{location, sample_files, FakeForge};

const PUB_A: &str = "4hMOQPTWOkE461Va0/ykByEMayRT+DM/4or9N+OA86Q=";
const PUB_B: &str = "vJWRze8CQnI5DrfEnTRNrKMhfPefAmHJ+yyXAnKDK1U=";

fn entry(id: &str, public: &str, state: &str) -> String {
    format!(r#"{{"id":"{id}","alg":"ed25519","public_key":"{public}","state":"{state}"}}"#)
}

fn file(entries: &[String]) -> String {
    format!(r#"{{"version":1,"keys":[{}]}}"#, entries.join(","))
}

fn signers_of(files: std::collections::BTreeMap<String, String>) -> Signers {
    let forge = FakeForge::new(files);
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let loaded = load(&forge, dir.path(), &location(), Policy::FailClosed, now).unwrap();
    from_loaded(&loaded, now)
}

fn with_signers(body: &str) -> std::collections::BTreeMap<String, String> {
    let mut f = sample_files();
    f.insert(DECISION_SIGNERS_PATH.to_string(), body.to_string());
    f
}

#[test]
fn keys_are_fetched_through_the_store_contract() {
    let s = signers_of(with_signers(&file(&[
        entry("dash-new", PUB_A, "active"),
        entry("dash-old", PUB_B, "revoked"),
    ])));
    assert!(s.is_loaded(), "{}", s.state);
    assert_eq!(s.keys.len(), 2);
    assert!(s.active_key("dash-new").is_some());
    assert!(s.active_key("dash-old").is_none(), "revoked never verifies");
    assert!(s.active_key("nope").is_none());
}

#[test]
fn two_active_keys_overlap_during_rotation() {
    let s = signers_of(with_signers(&file(&[
        entry("dash-a", PUB_A, "active"),
        entry("dash-b", PUB_B, "active"),
    ])));
    assert!(s.active_key("dash-a").is_some() && s.active_key("dash-b").is_some());
    assert_ne!(s.active_key("dash-a"), s.active_key("dash-b"));
}

#[test]
fn missing_file_is_unavailable() {
    let s = signers_of(sample_files());
    assert!(s.keys.is_empty() && !s.is_loaded());
    assert!(s.state.contains(DECISION_SIGNERS_PATH), "{}", s.state);
}

#[test]
fn every_invalid_file_rejects_the_whole_file() {
    let good = entry("dash-a", PUB_A, "active");
    let cases = [
        "{not json".to_string(),
        r#"{"version":2,"keys":[]}"#.to_string(),
        r#"{"keys":[]}"#.to_string(),
        format!(r#"{{"version":1,"keys":[{good}],"extra":1}}"#),
        format!(r#"{{"version":1,"version":1,"keys":[{good}]}}"#),
        file(&[good.replacen(r#""state""#, r#""note":"x","state""#, 1)]),
        file(&[good.clone(), entry("dash-a", PUB_B, "active")]),
        file(&[good.clone(), entry("dash-b", PUB_A, "revoked")]),
        file(&[entry("Dash-A", PUB_A, "active")]),
        file(&[entry("dash-a", PUB_A, "retired")]),
        file(&[good.replacen("ed25519", "rsa", 1)]),
        file(&[entry("dash-a", PUB_A.trim_end_matches('='), "active")]),
        file(&[entry("dash-a", "AAAA", "active")]),
        file(&[entry("dash-a", &PUB_A.replace('/', "_"), "active")]),
        file(
            &(0..=MAX_KEYS)
                .map(|i| entry(&format!("k{i}"), PUB_A, "active"))
                .collect::<Vec<_>>(),
        ),
    ];
    for body in cases {
        let s = signers_of(with_signers(&body));
        assert!(!s.is_loaded() && s.keys.is_empty(), "{body}: {}", s.state);
    }
}

#[test]
fn stale_cache_beyond_max_age_is_unavailable() {
    let forge = FakeForge::new(with_signers(&file(&[entry("dash-a", PUB_A, "active")])));
    let dir = tempfile::tempdir().unwrap();
    let t0 = chrono::Utc::now();
    load(&forge, dir.path(), &location(), Policy::FailClosed, t0).unwrap();
    forge.offline.set(true);
    let soon = t0 + chrono::Duration::hours(1);
    let s = from_loaded(
        &load(&forge, dir.path(), &location(), Policy::AllowStale, soon).unwrap(),
        soon,
    );
    assert!(s.is_loaded(), "a young stale cache is served: {}", s.state);
    let late = t0 + chrono::Duration::hours(25);
    let s = from_loaded(
        &load(&forge, dir.path(), &location(), Policy::AllowStale, late).unwrap(),
        late,
    );
    assert!(s.keys.is_empty() && s.state.contains("24h"), "{}", s.state);
}

#[test]
fn second_resolve_within_ttl_does_not_fetch() {
    let cache = Cache::default();
    let fetches = Cell::new(0);
    let load = || {
        fetches.set(fetches.get() + 1);
        Signers::unavailable("x")
    };
    let ttl = Duration::from_secs(300);
    cache.get_or_load("a/b@main", ttl, load);
    cache.get_or_load("a/b@main", ttl, load);
    assert_eq!(fetches.get(), 1);
    cache.get_or_load("a/b@main", Duration::ZERO, load);
    assert_eq!(fetches.get(), 2, "an expired entry reloads");
}
