//! Tests for the memoised budget reading (#11301): a counting fake probe,
//! an injected clock, and a memo of the test's own.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::budget::Budget;
use super::budget_cache::{
    read_in, ttl_from, BudgetMemo, CredentialFacts, CredentialKey, DEFAULT_TTL,
};
use crate::rate_limit_breaker::report::FailureContext;

const WRITER: &str = "/w/.loom/gh-config-by-owner/acme";
const READER_1: &str = "/w/.loom/gh-config-by-owner/acme/1111";
const READER_2: &str = "/w/.loom/gh-config-by-owner/acme/2222";

fn reading(core: u64, graphql: u64) -> Budget {
    Budget {
        core_remaining: core,
        graphql_remaining: graphql,
        cached: false,
    }
}

fn facts(dir: Option<&str>) -> CredentialFacts {
    CredentialFacts {
        config_dir: dir.map(PathBuf::from),
        ..CredentialFacts::default()
    }
}

fn key(root: &str, dir: Option<&str>) -> CredentialKey {
    CredentialKey::from_facts(Path::new(root), Some("gh"), facts(dir))
}

/// A memo, its clock's origin, and a probe that counts its calls.
struct Rig {
    memo: Mutex<BudgetMemo>,
    t0: Instant,
    probes: Cell<usize>,
}

impl Rig {
    fn new() -> Self {
        Self {
            memo: Mutex::default(),
            t0: Instant::now(),
            probes: Cell::new(0),
        }
    }

    /// Read `key` `secs` after the origin; the probe answers `answer`.
    fn read(&self, key: &CredentialKey, secs: u64, answer: Option<Budget>) -> Option<Budget> {
        self.read_ttl(key, secs, DEFAULT_TTL, answer)
    }

    fn read_ttl(
        &self,
        key: &CredentialKey,
        secs: u64,
        ttl: Duration,
        answer: Option<Budget>,
    ) -> Option<Budget> {
        let now = self.t0 + Duration::from_secs(secs);
        read_in(&self.memo, key, now, ttl, || {
            self.probes.set(self.probes.get() + 1);
            answer
        })
    }

    fn note_graphql(&self, key: &CredentialKey, remaining: u64) {
        self.memo.lock().unwrap().note_graphql(key, remaining);
    }
}

// --- sharing ------------------------------------------------------------------

#[test]
fn roots_on_one_credential_share_one_probe_within_the_ttl() {
    let rig = Rig::new();
    let roots = ["/w/a", "/w/b", "/w/c", "/w/d", "/w/e"];
    let mut answers = Vec::new();
    for (i, root) in (0u64..).zip(roots) {
        answers.push(rig.read(&key(root, Some(WRITER)), i * 20, Some(reading(4000, 3000))));
    }
    assert_eq!(rig.probes.get(), 1, "five roots, one writer App: one probe");
    assert_eq!(answers[0], Some(reading(4000, 3000)), "the probe's own answer");
    for later in &answers[1..] {
        let b = later.unwrap();
        assert!(b.cached, "a later root is answered from the memo: {b:?}");
        assert_eq!((b.core_remaining, b.graphql_remaining), (4000, 3000));
    }
}

#[test]
fn two_credentials_never_share_a_reading() {
    let rig = Rig::new();
    let one = rig.read(&key("/w/a", Some(READER_1)), 0, Some(reading(4000, 100)));
    let two = rig.read(&key("/w/b", Some(READER_2)), 1, Some(reading(4000, 4900)));
    let writer = rig.read(&key("/w/c", Some(WRITER)), 2, Some(reading(4000, 2500)));
    assert_eq!(rig.probes.get(), 3, "reader-1, reader-2 and the writer each probe");
    assert_eq!(one.unwrap().graphql_remaining, 100);
    assert_eq!(two.unwrap().graphql_remaining, 4900);
    assert_eq!(writer.unwrap().graphql_remaining, 2500);
    // And each later read gets its own credential's figure back.
    let again = rig.read(&key("/w/d", Some(READER_2)), 3, None).unwrap();
    assert_eq!((again.graphql_remaining, again.cached), (4900, true));
    assert_eq!(rig.probes.get(), 3);
}

// --- freshness ----------------------------------------------------------------

#[test]
fn an_expired_reading_is_probed_again() {
    let rig = Rig::new();
    let k = key("/w/a", Some(WRITER));
    rig.read(&k, 0, Some(reading(4000, 3000)));
    let ttl = DEFAULT_TTL.as_secs();
    assert!(rig.read(&k, ttl - 1, None).unwrap().cached, "still fresh one second before");
    assert_eq!(rig.probes.get(), 1);
    let renewed = rig.read(&k, ttl, Some(reading(3900, 2900))).unwrap();
    assert_eq!(rig.probes.get(), 2, "a reading exactly TTL old is expired");
    assert_eq!(renewed, reading(3900, 2900));
    assert!(rig.read(&k, ttl + 1, None).unwrap().cached, "the new reading is stored");
    assert_eq!(rig.probes.get(), 2);
}

#[test]
fn a_failed_probe_is_not_cached() {
    let rig = Rig::new();
    let k = key("/w/a", Some(WRITER));
    assert_eq!(rig.read(&k, 0, None), None);
    assert_eq!(rig.read(&k, 1, None), None);
    assert_eq!(rig.probes.get(), 2, "every run after a failure probes again");
    assert_eq!(rig.read(&k, 2, Some(reading(4000, 3000))), Some(reading(4000, 3000)));
    assert!(rig.read(&k, 3, None).unwrap().cached);
    assert_eq!(rig.probes.get(), 3);
}

#[test]
fn a_zero_ttl_turns_the_memo_off() {
    let rig = Rig::new();
    let k = key("/w/a", Some(WRITER));
    for secs in 0..3 {
        let b = rig.read_ttl(&k, secs, Duration::ZERO, Some(reading(4000, 3000)));
        assert_eq!(b, Some(reading(4000, 3000)));
    }
    assert_eq!(rig.probes.get(), 3);
}

#[test]
fn the_ttl_is_120_seconds_unless_the_environment_says_otherwise() {
    assert_eq!(DEFAULT_TTL, Duration::from_secs(120));
    assert_eq!(ttl_from(None), DEFAULT_TTL);
    assert_eq!(ttl_from(Some(" 30 ")), Duration::from_secs(30));
    assert_eq!(ttl_from(Some("0")), Duration::ZERO);
    assert_eq!(ttl_from(Some("soon")), DEFAULT_TTL, "an unparsable value keeps the default");
}

// --- refresh from the run's own answers ----------------------------------------

#[test]
fn a_graphql_answer_refreshes_the_cached_figure() {
    let rig = Rig::new();
    let k = key("/w/a", Some(WRITER));
    rig.read(&k, 0, Some(reading(4000, 3000)));
    rig.note_graphql(&k, 2400);
    let b = rig.read(&key("/w/b", Some(WRITER)), 60, None).unwrap();
    assert_eq!(
        (b.core_remaining, b.graphql_remaining, b.cached),
        (4000, 2400, true),
        "a later root sees the lower figure"
    );
    assert_eq!(rig.probes.get(), 1);
}

#[test]
fn a_graphql_answer_does_not_extend_the_reading() {
    let rig = Rig::new();
    let k = key("/w/a", Some(WRITER));
    rig.read(&k, 0, Some(reading(4000, 3000)));
    rig.note_graphql(&k, 2400);
    let ttl = DEFAULT_TTL.as_secs();
    rig.read(&k, ttl, Some(reading(3800, 2300)));
    assert_eq!(rig.probes.get(), 2, "the core figure is as old as the probe");
}

#[test]
fn a_graphql_answer_with_nothing_stored_is_dropped() {
    let rig = Rig::new();
    let k = key("/w/a", Some(WRITER));
    rig.note_graphql(&k, 2400);
    assert_eq!(rig.read(&k, 1, Some(reading(4000, 3000))), Some(reading(4000, 3000)));
    assert_eq!(rig.probes.get(), 1, "a GraphQL figure alone is not a reading");
}

#[test]
fn a_graphql_answer_on_another_credential_changes_nothing() {
    let rig = Rig::new();
    rig.read(&key("/w/a", Some(READER_1)), 0, Some(reading(4000, 3000)));
    rig.note_graphql(&key("/w/a", Some(READER_2)), 10);
    let b = rig.read(&key("/w/b", Some(READER_1)), 1, None).unwrap();
    assert_eq!(b.graphql_remaining, 3000);
}

// --- the key ------------------------------------------------------------------

#[test]
fn the_key_is_the_credential_not_the_root() {
    assert_eq!(key("/w/a", Some(WRITER)), key("/w/b", Some(WRITER)));
    assert_ne!(key("/w/a", Some(READER_1)), key("/w/a", Some(READER_2)));
    assert_ne!(key("/w/a", Some(WRITER)), key("/w/a", Some(READER_1)));
    let stub =
        CredentialKey::from_facts(Path::new("/w/a"), Some("/t/stub-gh"), facts(Some(WRITER)));
    assert_ne!(stub, key("/w/a", Some(WRITER)), "another gh program is another credential");
}

#[test]
fn a_credential_that_cannot_be_named_keys_by_root() {
    let by_root = |root: &str, f: CredentialFacts| {
        let k = CredentialKey::from_facts(Path::new(root), Some("gh"), f);
        assert!(matches!(k, CredentialKey::Root { .. }), "{k:?}");
        k
    };
    // No GH_CONFIG_DIR at all: gh's default profile.
    assert_ne!(by_root("/w/a", facts(None)), by_root("/w/b", facts(None)));
    // An env token outranks the directory.
    let token = CredentialFacts {
        env_token: true,
        ..facts(Some(WRITER))
    };
    assert_ne!(by_root("/w/a", token.clone()), by_root("/w/b", token));
    // A forge-egress launcher picks its own per-owner credential.
    let launcher = CredentialFacts {
        launcher: true,
        ..facts(Some(WRITER))
    };
    assert_ne!(by_root("/w/a", launcher.clone()), by_root("/w/b", launcher));
}

#[test]
fn the_key_names_the_probes_own_config_dir() {
    let ctx = FailureContext {
        config_dir: Some(PathBuf::from(READER_1)),
        ..FailureContext::for_root("/w/a", "gh")
    };
    assert_eq!(
        crate::rate_limit_breaker::forge::probe_config_dir(&ctx),
        Some(PathBuf::from(READER_1)),
        "an explicit credential dir is the one the probe runs under"
    );
}

#[test]
fn budget_before_says_when_it_was_memoised() {
    let v = serde_json::to_value(Budget {
        cached: true,
        ..reading(4000, 3000)
    })
    .unwrap();
    assert_eq!(v["cached"], true);
    assert_eq!(v["graphql_remaining"], 3000);
}
