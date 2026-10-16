use std::io::{Read as _, Write as _};
use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use serial_test::serial;

use super::mirror::Fetched;
use super::*;
use crate::forge_mirror::client::{Conditional, MirrorError};
use anyhow::anyhow;

/// The fallback tiers' backstop pace in these tests.
const BACKSTOP: Duration = Duration::from_secs(3600);

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-16T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn target(slug: &str) -> Target {
    Target {
        slug: slug.to_string(),
        gh_bin: PathBuf::from("gh"),
        root_dir: PathBuf::from("."),
    }
}

fn worklist_body() -> Value {
    json!({ "repos": {
        "acme/widgets": {
            "watermark": {
                "known": true, "repo": "acme/widgets", "latest_seq": 9,
                "last_event_at": "2026-10-16T11:59:00Z",
                "last_reconciled_at": "2026-10-16T11:58:00Z",
            },
            "items": [
                { "number": 7, "type": "issue", "state": "open", "title": "an issue",
                  "labels": ["loom:issue"], "labels_at": { "loom:issue": "2026-10-16T10:00:00Z" },
                  "assignees": ["bob"], "author": "alice", "author_association": "MEMBER",
                  "created_at": "2026-10-15T00:00:00Z", "updated_at": "2026-10-16T10:00:00Z",
                  "sub_issues_total": 3, "sub_issues_completed": 1,
                  "blocked_by_total": 2, "blocked_by_open": 1 },
                { "number": 8, "type": "pr", "state": "open", "labels": ["loom:review-requested"],
                  "author": "carol", "author_association": "NONE", "body": "Closes #7",
                  "draft": true, "head_ref": "feature/issue-7", "head_sha": "abc123",
                  "head_repo": "acme/widgets", "base_ref": "main",
                  "checks": { "state": "pending", "started_at": "2026-10-16T11:00:00Z" } },
                { "number": 9, "type": "issue", "state": "closed", "labels": [] },
            ],
        },
        "acme/old": {
            "watermark": { "known": true, "repo": "acme/old", "latest_seq": 1,
                           "last_reconciled_at": "2026-10-16T08:00:00Z" },
            "items": [],
        },
    }})
}

/// The fleet chain with a worklist tier, whose direct listings are the
/// gateway tier's.
fn wl() -> Chain {
    Chain::fleet(Mode::Worklist, Tier::Gateway)
}

/// A fallback listing: the fixture's rows for `slug`, recorded under `tier`.
fn listed(slug: &str, tier: Tier) -> RepoSnapshot {
    let mut snap = worklist::decode(worklist_body())
        .unwrap()
        .repos
        .get(&slug.to_ascii_lowercase())
        .cloned()
        .map_or_else(
            || RepoSnapshot {
                slug: slug.to_string(),
                source: Source::Listing,
                tier,
                status: RepoStatus::Ok,
                rows: Vec::new(),
                pulls: Some(Vec::new()),
                extras: BTreeMap::new(),
                watermark: None,
            },
            |r| (*r).clone(),
        );
    snap.source = Source::Listing;
    snap.tier = tier;
    snap.watermark = None;
    snap
}

/// A lister that answers every repo and counts its calls.
fn counting(
    calls: Arc<AtomicU64>,
) -> impl Fn(&Target, &AtomicU64, bool, Tier) -> Result<RepoSnapshot> {
    move |t, reads, per_call, tier| {
        assert!(!per_call, "a fallback tier never asks the mirror per call");
        calls.fetch_add(1, Ordering::SeqCst);
        reads.fetch_add(1, Ordering::Relaxed);
        Ok(listed(&t.slug, tier))
    }
}

fn modified(etag: &str) -> Fetched {
    Fetched {
        outcome: Ok(Conditional::Modified {
            etag: Some(etag.to_string()),
            body: worklist_body(),
        }),
        latency: Duration::from_millis(12),
    }
}

fn failed(status: u16) -> Fetched {
    Fetched {
        outcome: Err(MirrorError::Status(status)),
        latency: Duration::from_millis(3),
    }
}

fn mirror_block() -> Value {
    json!({ "origin": "https://events.example.com", "keyRef": "file:/tmp/loom-test-key" })
}

fn config(discovery: Option<&str>) -> Value {
    let mut block = mirror_block();
    if let Some(d) = discovery {
        block["discovery"] = json!(d);
    }
    json!({ "forge": { "mirror": block } })
}

#[test]
fn no_mirror_block_is_the_gh_only_chain() {
    for effective in [
        json!({}),
        // The mode alone configures nothing.
        json!({ "forge": { "mirror": { "discovery": "worklist" } } }),
        json!({ "forge": { "mirror": null } }),
    ] {
        let chain = Chain::from_effective(&effective);
        assert_eq!(chain.tiers(), vec![Tier::Gh], "{effective}");
        assert_eq!(chain.backend(), BackendKind::Direct);
        assert!(!chain.per_call_mirror());
        assert!(!chain.is_worklist());
    }
}

#[test]
fn a_mirror_block_selects_the_chain_by_discovery_mode() {
    let chain = |d: Option<&str>| Chain::from_effective(&config(d));
    // The default during the transition is today's per-call behaviour.
    let per_call = chain(None);
    assert_eq!(per_call.mode(), Some(Mode::PerCall));
    assert!(per_call.per_call_mirror());
    assert_eq!(per_call.backend(), BackendKind::Direct);
    assert_eq!(chain(Some("per_call")).mode(), Some(Mode::PerCall));
    assert_eq!(chain(Some("garbage")).mode(), Some(Mode::PerCall));

    let worklist = chain(Some("worklist"));
    assert_eq!(worklist.tiers(), vec![Tier::Mirror, Tier::Gateway, Tier::Gh]);
    assert_eq!(worklist.backend(), BackendKind::Mirror);
    assert!(!worklist.per_call_mirror());

    // Break-glass: a mirror that is wrong, not down.
    let direct = chain(Some(" Direct "));
    assert_eq!(direct.tiers(), vec![Tier::Gateway, Tier::Gh]);
    assert_eq!(direct.backend(), BackendKind::Direct);
    assert!(!direct.per_call_mirror(), "the break-glass never asks the mirror");
}

#[test]
fn the_listing_tier_is_the_gateway_when_one_is_configured() {
    let mut effective = config(Some("worklist"));
    assert_eq!(Chain::from_effective(&effective).listing_tier(), Tier::Gh);
    effective["forge"]["gateway"] = json!({
        "origin": "https://gateway.example.com",
        "keyRefs": { "acme": "file:/tmp/loom-test-gateway-key" },
    });
    assert_eq!(Chain::from_effective(&effective).listing_tier(), Tier::Gateway);
}

#[test]
fn the_alias_key_from_the_first_slice_still_means_worklist() {
    let mut effective = config(None);
    effective["autonomous"] = json!({ "workFinder": { "discovery": "mirror" } });
    assert_eq!(Chain::from_effective(&effective).mode(), Some(Mode::Worklist));
    // The new key wins.
    effective["forge"]["mirror"]["discovery"] = json!("direct");
    assert_eq!(Chain::from_effective(&effective).mode(), Some(Mode::Direct));
    // Without a mirror there is nothing to be a worklist of.
    let bare = json!({ "autonomous": { "workFinder": { "discovery": "mirror" } } });
    assert_eq!(Chain::from_effective(&bare).tiers(), vec![Tier::Gh]);
}

#[test]
fn a_worklist_converts_to_rows_pulls_and_extras() {
    let mut d = Discovery::with_backstop(BACKSTOP);
    let snap = d.begin_mirror_tick_with(wl(), now(), |etag| {
        assert_eq!(etag, None, "the first request is unconditional");
        modified("\"w1\"")
    });
    let repo = snap.repo(&target("Acme/Widgets")).unwrap();
    assert_eq!(repo.source, Source::Worklist);
    // The closed item is not a row.
    let numbers: Vec<u32> = repo.rows.iter().map(|r| r.number).collect();
    assert_eq!(numbers, vec![7, 8]);
    let issue = &repo.rows[0];
    assert_eq!(issue.open_sub_issues, Some(2));
    assert_eq!(issue.author_association.as_deref(), Some("MEMBER"));
    assert_eq!(
        issue
            .labels_at
            .as_ref()
            .unwrap()
            .get("loom:issue")
            .map(String::as_str),
        Some("2026-10-16T10:00:00Z")
    );
    assert!(repo.rows[1].is_pull_request);
    let pulls = repo.pulls.as_ref().unwrap();
    assert_eq!(pulls.len(), 1);
    assert!(pulls[0].draft);
    assert_eq!(pulls[0].head_sha.as_deref(), Some("abc123"));
    assert_eq!(pulls[0].head_ref.as_deref(), Some("feature/issue-7"));
    assert_eq!(pulls[0].base_ref.as_deref(), Some("main"));
    assert_eq!(pulls[0].head_repo.as_deref(), Some("acme/widgets"));
    let extras = &repo.extras;
    assert_eq!(extras[&7].assignees, vec!["bob".to_string()]);
    assert_eq!(extras[&7].blocked_by_total, Some(2));
    assert_eq!(extras[&7].blocked_by_open, Some(1));
    assert_eq!(extras[&8].checks.as_ref().unwrap().state, "pending");
    assert!(extras[&7].checks.is_none());
}

#[test]
fn a_tick_makes_one_request_and_reads_no_forge_listing() {
    let mut d = Discovery::with_backstop(BACKSTOP);
    let mut requests = 0;
    let snap = d.begin_mirror_tick_with(wl(), now(), |_| {
        requests += 1;
        modified("\"w1\"")
    });
    snap.repo(&target("acme/widgets")).unwrap();
    snap.repo(&target("acme/old")).unwrap();
    assert_eq!(requests, 1, "one worklist request serves every repo");
    let stats = snap.stats();
    assert_eq!(stats.backend, "mirror");
    assert_eq!(stats.discovery_forge_reads, 0);
    assert_eq!(stats.worklist_latency_ms, Some(12));
    assert_eq!(stats.not_modified, Some(false));
    assert_eq!(stats.repos, 2);
    assert_eq!(stats.items, 2);
}

#[test]
fn an_unchanged_etag_reuses_the_previous_snapshot() {
    let mut d = Discovery::with_backstop(BACKSTOP);
    d.begin_mirror_tick_with(wl(), now(), |_| modified("\"w1\""));
    let snap = d.begin_mirror_tick_with(wl(), now(), |etag| {
        assert_eq!(etag, Some("\"w1\""), "the next request is conditional");
        Fetched {
            outcome: Ok(Conditional::NotModified),
            latency: Duration::from_millis(2),
        }
    });
    assert_eq!(snap.repo(&target("acme/widgets")).unwrap().rows.len(), 2);
    let stats = snap.stats();
    assert_eq!(stats.not_modified, Some(true));
    assert_eq!(stats.discovery_forge_reads, 0);
}

#[test]
fn a_ready_worklist_serves_every_repo_from_the_mirror_tier() {
    let mut d = Discovery::default();
    let snap = d
        .begin_mirror_tick_with(wl(), now(), |_| modified("\"w1\""))
        .with_lister(|_, _, _, _| panic!("a worklist answer lists nothing"));
    assert_eq!(snap.repo(&target("acme/widgets")).unwrap().tier, Tier::Mirror);
    snap.repo(&target("acme/old")).unwrap();
    let stats = snap.stats();
    assert_eq!(stats.tier, "mirror");
    assert_eq!(stats.worklist_outcome, Some("ok"));
    assert_eq!(stats.fallback_reason, None);
    assert!(stats.fallback_repos.is_empty());
    assert_eq!((stats.repos, stats.repos_ok), (2, 2));
    assert_eq!(stats.discovery_forge_reads, 0);
}

#[test]
fn a_not_modified_tick_has_the_rows_of_the_tick_before() {
    let mut d = Discovery::default();
    let first = d.begin_mirror_tick_with(wl(), now(), |_| modified("\"w1\""));
    let before = first.repo(&target("acme/widgets")).unwrap();
    let second = d.begin_mirror_tick_with(wl(), now(), |_| Fetched {
        outcome: Ok(Conditional::NotModified),
        latency: Duration::ZERO,
    });
    let after = second.repo(&target("acme/widgets")).unwrap();
    assert_eq!(second.stats().worklist_outcome, Some("not_modified"));
    assert!(Arc::ptr_eq(&before, &after), "the one cached body, not a copy");
}

#[test]
fn a_failed_worklist_falls_to_the_gateway_for_every_repo_and_the_span_says_so() {
    let mut d = Discovery::default();
    d.begin_mirror_tick_with(wl(), now(), |_| modified("\"w1\""));
    let calls = Arc::new(AtomicU64::new(0));
    let snap = d
        .begin_mirror_tick_with(wl(), now(), |_| failed(503))
        .with_lister(counting(calls.clone()));
    // The previous body is not served: the failed tick lists instead.
    for slug in ["acme/widgets", "acme/old"] {
        let repo = snap.repo(&target(slug)).unwrap();
        assert_eq!((repo.tier, repo.source), (Tier::Gateway, Source::Listing));
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let stats = snap.stats();
    assert_eq!(stats.tier, "gateway");
    assert_eq!(stats.fallback_reason.as_deref(), Some("status"));
    assert_eq!(stats.fallback_repos, vec!["acme/widgets", "acme/old"]);
    assert_eq!(stats.worklist_outcome, Some("error"));
    assert_eq!(stats.worklist_error_class, Some("status"));
    assert_eq!(stats.discovery_forge_reads, 2);
}

#[test]
fn every_worklist_failure_class_is_named() {
    let class = |outcome: Result<Conditional, MirrorError>| {
        let mut d = Discovery::default();
        let snap = d.begin_mirror_tick_with(wl(), now(), |_| Fetched {
            outcome,
            latency: Duration::ZERO,
        });
        snap.stats().worklist_error_class
    };
    assert_eq!(class(Err(MirrorError::Transport("timeout".into()))), Some("transport"));
    assert_eq!(class(Err(MirrorError::Auth(401))), Some("auth"));
    assert_eq!(class(Err(MirrorError::Status(502))), Some("status"));
    assert_eq!(class(Err(MirrorError::Malformed("x".into()))), Some("malformed"));
    assert_eq!(
        class(Ok(Conditional::NotModified)),
        Some("malformed"),
        "a 304 with nothing cached"
    );
}

#[test]
fn a_malformed_worklist_falls_to_the_gateway() {
    let mut d = Discovery::default();
    let snap = d
        .begin_mirror_tick_with(wl(), now(), |_| Fetched {
            outcome: Ok(Conditional::Modified {
                etag: None,
                body: json!({ "repos": "nope" }),
            }),
            latency: Duration::ZERO,
        })
        .with_lister(counting(Arc::default()));
    assert_eq!(snap.repo(&target("acme/widgets")).unwrap().tier, Tier::Gateway);
    assert_eq!(snap.stats().worklist_error_class, Some("malformed"));
}

#[test]
fn a_status_that_is_not_ok_falls_to_the_gateway_for_that_repo_alone() {
    let mut body = worklist_body();
    body["repos"]["acme/old"]["status"] = json!("backfilling");
    let mut d = Discovery::default();
    let calls = Arc::new(AtomicU64::new(0));
    let snap = d
        .begin_mirror_tick_with(wl(), now(), |_| Fetched {
            outcome: Ok(Conditional::Modified {
                etag: Some("\"w2\"".into()),
                body,
            }),
            latency: Duration::ZERO,
        })
        .with_lister(counting(calls.clone()));
    assert_eq!(snap.repo(&target("acme/widgets")).unwrap().tier, Tier::Mirror);
    assert_eq!(snap.repo(&target("acme/old")).unwrap().tier, Tier::Gateway);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "only the repo the mirror could not answer for");
    let stats = snap.stats();
    assert_eq!(stats.tier, "gateway");
    assert_eq!(stats.fallback_reason.as_deref(), Some("repo_status"));
    assert_eq!(stats.fallback_repos, vec!["acme/old"]);
    assert_eq!((stats.repos, stats.repos_ok, stats.repos_backfilling), (2, 1, 1));
    assert_eq!(stats.worklist_outcome, Some("ok"), "the request itself answered");
}

#[test]
fn unknown_and_error_statuses_and_unrecognised_ones_all_fall_back() {
    for (wire, bucket) in [
        ("unknown", "unknown"),
        ("error", "error"),
        ("weird", "error"),
    ] {
        let mut body = worklist_body();
        body["repos"]["acme/old"]["status"] = json!(wire);
        let mut d = Discovery::default();
        let snap = d
            .begin_mirror_tick_with(wl(), now(), |_| Fetched {
                outcome: Ok(Conditional::Modified { etag: None, body }),
                latency: Duration::ZERO,
            })
            .with_lister(counting(Arc::default()));
        assert_eq!(snap.repo(&target("acme/old")).unwrap().tier, Tier::Gateway, "{wire}");
        let stats = snap.stats();
        let got = (stats.repos_unknown, stats.repos_error);
        assert_eq!(got, if bucket == "unknown" { (1, 0) } else { (0, 1) }, "{wire}");
    }
}

#[test]
fn a_repo_the_key_cannot_read_falls_back_for_that_repo_only() {
    let mut d = Discovery::default();
    let snap = d
        .begin_mirror_tick_with(wl(), now(), |_| modified("\"w1\""))
        .with_lister(counting(Arc::default()));
    assert_eq!(snap.repo(&target("acme/secret")).unwrap().tier, Tier::Gateway);
    assert_eq!(snap.repo(&target("acme/widgets")).unwrap().tier, Tier::Mirror);
    assert_eq!(snap.stats().repos_unknown, 1);
}

#[test]
fn two_consecutive_gateway_ticks_list_once_and_the_second_draws_from_memory() {
    let mut d = Discovery::default();
    let calls = Arc::new(AtomicU64::new(0));
    let tick = |d: &mut Discovery| {
        d.begin_mirror_tick_with(wl(), now(), |_| failed(503))
            .with_lister(counting(calls.clone()))
    };
    let first = tick(&mut d);
    let a = first.repo(&target("acme/widgets")).unwrap();
    let second = tick(&mut d);
    let b = second.repo(&target("acme/widgets")).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the backstop pace, not every tick");
    assert!(Arc::ptr_eq(&a, &b));
    let stats = second.stats();
    assert_eq!(stats.tier, "gateway");
    assert!(stats.fallback_repos.is_empty(), "nothing was listed this tick");
    assert_eq!(stats.discovery_forge_reads, 0);
}

#[test]
fn a_repo_that_stays_unknown_is_listed_at_the_backstop_pace() {
    let mut d = Discovery::default();
    let calls = Arc::new(AtomicU64::new(0));
    for _ in 0..3 {
        let snap = d
            .begin_mirror_tick_with(wl(), now(), |_| modified("\"w1\""))
            .with_lister(counting(calls.clone()));
        snap.repo(&target("acme/widgets")).unwrap();
        snap.repo(&target("acme/secret")).unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn past_the_backstop_the_repo_is_listed_again() {
    let mut d = Discovery::with_backstop(Duration::ZERO);
    let calls = Arc::new(AtomicU64::new(0));
    for _ in 0..2 {
        let snap = d
            .begin_mirror_tick_with(wl(), now(), |_| failed(503))
            .with_lister(counting(calls.clone()));
        snap.repo(&target("acme/widgets")).unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn a_failed_listing_is_tier_none_and_is_not_retried_until_the_backstop() {
    let mut d = Discovery::default();
    let calls = Arc::new(AtomicU64::new(0));
    let failing = |calls: Arc<AtomicU64>| {
        move |_: &Target, _: &AtomicU64, _: bool, _: Tier| -> Result<RepoSnapshot> {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(anyhow!("no credential"))
        }
    };
    for _ in 0..2 {
        let snap = d
            .begin_mirror_tick_with(wl(), now(), |_| failed(503))
            .with_lister(failing(calls.clone()));
        let err = snap.repo(&target("acme/widgets")).unwrap_err();
        assert!(format!("{err:#}").contains("no tier answered"), "{err:#}");
        let stats = snap.stats();
        assert_eq!(stats.tier, "none");
        // Why the repo left the first tier, not why the next one failed.
        assert_eq!(stats.fallback_reason.as_deref(), Some("status"));
        assert!(snap.repos().is_empty(), "no snapshot, so no draw");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_reserve_refusal_ends_discovery_draws_from_the_last_listing_and_spares_the_breaker() {
    use crate::gh_invocation::purpose;
    let mut d = Discovery::with_backstop(Duration::ZERO);
    let calls = Arc::new(AtomicU64::new(0));
    // Tick 1 lists both repos.
    let snap = d
        .begin_mirror_tick_with(wl(), now(), |_| failed(503))
        .with_lister(counting(calls.clone()));
    let last = snap.repo(&target("acme/widgets")).unwrap();
    snap.repo(&target("acme/old")).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);

    // Tick 2: the gateway refuses the first listing for its reserve.
    let attempts = Arc::new(AtomicU64::new(0));
    let refusing = {
        let attempts = attempts.clone();
        move |_: &Target, _: &AtomicU64, _: bool, _: Tier| -> Result<RepoSnapshot> {
            attempts.fetch_add(1, Ordering::SeqCst);
            purpose::end_discovery();
            Err(anyhow!("gh: loom-gateway-reserve: discovery refused (HTTP 429)"))
        }
    };
    let breaker_before =
        format!("{:?}", crate::rate_limit_breaker::global().map(|b| b.is_suppressed(Utc::now())));
    let snap = d
        .begin_mirror_tick_with(wl(), now(), |_| failed(503))
        .with_lister(refusing);
    let again = snap.repo(&target("acme/widgets")).unwrap();
    assert!(Arc::ptr_eq(&last, &again), "drawn from the last listing");
    snap.repo(&target("acme/old")).unwrap();
    purpose::reopen_discovery();
    assert_eq!(attempts.load(Ordering::SeqCst), 1, "the refusal ended the tick's discovery");
    let stats = snap.stats();
    assert_eq!(stats.fallback_reason.as_deref(), Some("reserve"));
    assert_eq!(stats.tier, "gateway");
    let breaker_after =
        format!("{:?}", crate::rate_limit_breaker::global().map(|b| b.is_suppressed(Utc::now())));
    assert_eq!(breaker_before, breaker_after, "a reserve refusal leaves the breaker alone");

    // Tick 3 tries the first tier again.
    let snap = d.begin_mirror_tick_with(wl(), now(), |_| modified("\"w1\""));
    assert_eq!(snap.repo(&target("acme/widgets")).unwrap().tier, Tier::Mirror);
}

#[test]
fn a_reserve_refusal_with_no_earlier_listing_draws_nothing() {
    use crate::gh_invocation::purpose;
    let mut d = Discovery::default();
    let snap = d
        .begin_mirror_tick_with(wl(), now(), |_| failed(503))
        .with_lister(|_, _, _, _| {
            purpose::end_discovery();
            Err(anyhow!("loom-gateway-reserve"))
        });
    assert!(snap.repo(&target("acme/widgets")).is_err());
    purpose::reopen_discovery();
    let stats = snap.stats();
    assert_eq!((stats.tier, stats.fallback_reason.as_deref()), ("none", Some("reserve")));
}

#[test]
fn the_standalone_chain_makes_no_mirror_or_gateway_request() {
    let mut d = Discovery::default();
    let snap = d
        .begin_tick(Chain::standalone(), now())
        .with_lister(|t, reads, per_call, tier| {
            assert!(!per_call);
            reads.fetch_add(1, Ordering::Relaxed);
            Ok(listed(&t.slug, tier))
        });
    let repo = snap.repo(&target("acme/widgets")).unwrap();
    assert_eq!(repo.tier, Tier::Gh);
    let stats = snap.stats();
    assert_eq!((stats.backend, stats.tier), ("direct", "gh"));
    assert_eq!(stats.worklist_outcome, None, "no worklist was asked");
    assert_eq!(stats.discovery_forge_reads, 1);
}

#[test]
fn the_direct_chains_list_every_tick_with_no_pace() {
    let mut d = Discovery::default();
    let calls = Arc::new(AtomicU64::new(0));
    for _ in 0..2 {
        let snap = d
            .begin_tick(Chain::fleet(Mode::Direct, Tier::Gateway), now())
            .with_lister(counting(calls.clone()));
        assert_eq!(snap.repo(&target("acme/widgets")).unwrap().tier, Tier::Gateway);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[test]
fn only_the_per_call_chain_lets_the_listing_ask_the_mirror() {
    let mut d = Discovery::default();
    let saw = Arc::new(Mutex::new(Vec::new()));
    for chain in [
        Chain::standalone(),
        Chain::fleet(Mode::PerCall, Tier::Gateway),
        Chain::fleet(Mode::Direct, Tier::Gateway),
    ] {
        let saw = saw.clone();
        let snap = d
            .begin_tick(chain, now())
            .with_lister(move |t, _, per_call, tier| {
                saw.lock().unwrap().push(per_call);
                Ok(listed(&t.slug, tier))
            });
        snap.repo(&target("acme/widgets")).unwrap();
    }
    assert_eq!(*saw.lock().unwrap(), vec![false, true, false]);
}

#[test]
fn a_stale_repo_is_served_with_its_watermark_recorded() {
    let mut d = Discovery::with_backstop(BACKSTOP);
    let snap = d.begin_mirror_tick_with(wl(), now(), |_| modified("\"w1\""));
    // acme/old was last reconciled four hours ago: past the 600 s bound.
    let old = snap.repo(&target("acme/old")).unwrap();
    assert!(old.rows.is_empty());
    let mark = old.watermark.as_ref().unwrap();
    assert_eq!(mark.age_secs(now()), Some(4 * 3600));
    snap.repo(&target("acme/widgets")).unwrap();
    let stats = snap.stats();
    assert_eq!(stats.tier, "mirror", "stale is served, not refused");
    assert_eq!(stats.oldest_watermark_age_secs, Some(4 * 3600));
    assert_eq!(stats.stale_repos, 1);
    assert_eq!(stats.discovery_forge_reads, 0);
}

#[test]
fn a_repo_the_mirror_cannot_vouch_for_is_not_served_but_others_are() {
    type Edit = fn(&mut Value);
    let cases: [(&str, Edit); 6] = [
        ("known false", |w| w["known"] = json!(false)),
        ("known absent", |w| {
            w.as_object_mut().unwrap().remove("known");
        }),
        ("gone", |w| w["gone"] = json!("moved to acme/new")),
        ("another repo", |w| w["repo"] = json!("acme/other")),
        ("no repo named", |w| {
            w.as_object_mut().unwrap().remove("repo");
        }),
        ("never reconciled", |w| {
            w.as_object_mut().unwrap().remove("last_reconciled_at");
        }),
    ];
    for (why, edit) in cases {
        let mut body = worklist_body();
        edit(&mut body["repos"]["acme/old"]["watermark"]);
        let mut d = Discovery::default();
        let calls = Arc::new(AtomicU64::new(0));
        let snap = d
            .begin_mirror_tick_with(wl(), now(), |_| Fetched {
                outcome: Ok(Conditional::Modified {
                    etag: Some("\"w3\"".into()),
                    body,
                }),
                latency: Duration::ZERO,
            })
            .with_lister(counting(calls.clone()));
        assert_eq!(snap.repo(&target("acme/widgets")).unwrap().tier, Tier::Mirror, "{why}");
        assert_eq!(snap.repo(&target("acme/old")).unwrap().tier, Tier::Gateway, "{why}");
        assert_eq!(calls.load(Ordering::SeqCst), 1, "{why}: only the unvouched repo falls back");
        assert_eq!(snap.stats().fallback_repos, vec!["acme/old"], "{why}");
    }
}

#[test]
fn an_uppercase_repo_in_the_watermark_is_still_vouched_for() {
    let mut body = worklist_body();
    body["repos"]["acme/old"]["watermark"]["repo"] = json!("ACME/Old");
    let mut d = Discovery::default();
    let snap = d.begin_mirror_tick_with(wl(), now(), |_| Fetched {
        outcome: Ok(Conditional::Modified {
            etag: Some("\"w4\"".into()),
            body,
        }),
        latency: Duration::ZERO,
    });
    assert_eq!(snap.repo(&target("acme/old")).unwrap().tier, Tier::Mirror);
}

#[test]
fn the_repo_is_read_once_per_tick() {
    let mut d = Discovery::with_backstop(BACKSTOP);
    let snap = d.begin_mirror_tick_with(wl(), now(), |_| modified("\"w1\""));
    let a = snap.repo(&target("acme/widgets")).unwrap();
    let b = snap.repo(&target("ACME/WIDGETS")).unwrap();
    assert!(Arc::ptr_eq(&a, &b));
    assert_eq!(snap.repos().len(), 1);
}

#[test]
fn the_direct_backend_reports_no_worklist_facts_until_it_lists() {
    let mut d = Discovery::with_backstop(BACKSTOP);
    let snap = d.begin_tick(Chain::standalone(), now());
    let stats = snap.stats();
    assert_eq!(stats.backend, "direct");
    assert_eq!(stats.worklist_latency_ms, None);
    assert_eq!(stats.not_modified, None);
    assert_eq!(stats.discovery_forge_reads, 0);
}

#[test]
fn the_mirror_backend_never_reaches_the_per_call_listing_path() {
    for file in ["mirror.rs", "worklist.rs"] {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src/discovery")
            .join(file);
        let text = std::fs::read_to_string(path).unwrap();
        let code: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!code.contains("list_open"), "{file} must not call a per-repo listing");
        assert!(!code.contains("forge_mirror::discovery"), "{file}");
        assert!(!code.contains("forge_listing::list"), "{file}");
    }
}

// ----------------------------------------------------------- over HTTP

type Seen = Arc<Mutex<Vec<(String, Option<String>)>>>;

/// Answers each connection in turn: `(status, etag, body)`. Records the
/// request line and its `If-None-Match`.
fn serve(responses: Vec<(u16, Option<&'static str>, String)>) -> (String, Seen) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let seen_t = seen.clone();
    std::thread::spawn(move || {
        for (i, stream) in listener.incoming().enumerate() {
            let Ok(mut stream) = stream else { break };
            let mut buf = Vec::new();
            let mut b = [0u8; 1];
            while buf.len() < 16_384 && !buf.ends_with(b"\r\n\r\n") {
                match stream.read(&mut b) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => buf.push(b[0]),
                }
            }
            let head = String::from_utf8_lossy(&buf).into_owned();
            let mut lines = head.lines();
            let line = lines.next().unwrap_or_default().to_string();
            let inm = lines
                .find(|l| l.to_ascii_lowercase().starts_with("if-none-match:"))
                .map(|l| l[14..].trim().to_string());
            seen_t.lock().unwrap().push((line, inm));
            let (status, etag, body) = responses
                .get(i)
                .or_else(|| responses.last())
                .cloned()
                .unwrap();
            let etag = etag.map_or(String::new(), |e| format!("ETag: {e}\r\n"));
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{etag}Content-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        }
    });
    (format!("http://{addr}"), seen)
}

#[test]
#[serial]
fn against_a_fake_server_a_tick_is_one_request_and_the_next_is_a_304() {
    let (base, seen) = serve(vec![
        (200, Some("\"w1\""), worklist_body().to_string()),
        (304, None, String::new()),
        (503, None, "{}".to_string()),
    ]);
    crate::forge_mirror::set_active(Some(Arc::new(crate::forge_mirror::Active::for_test(
        &base,
        Duration::from_secs(600),
    ))));
    let mut d = Discovery::with_backstop(BACKSTOP);

    let first = d.begin_tick(wl(), now());
    assert_eq!(first.repo(&target("acme/widgets")).unwrap().rows.len(), 2);
    assert_eq!(first.stats().not_modified, Some(false));

    let second = d.begin_tick(wl(), now());
    assert_eq!(second.repo(&target("acme/widgets")).unwrap().rows.len(), 2);
    assert_eq!(second.stats().not_modified, Some(true));
    assert_eq!(second.stats().discovery_forge_reads, 0);

    // A 5xx falls to the next tier rather than serving the old body.
    let third = d
        .begin_tick(wl(), now())
        .with_lister(counting(Arc::default()));
    assert_eq!(third.repo(&target("acme/widgets")).unwrap().tier, Tier::Gateway);
    assert_eq!(third.stats().worklist_outcome, Some("error"));

    crate::forge_mirror::set_active(None);
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3, "one request per tick, nothing else");
    assert!(
        seen.iter()
            .all(|(line, _)| line.starts_with("GET /v1/mirror/worklist ")),
        "{seen:?}"
    );
    assert_eq!(seen[0].1, None);
    assert_eq!(seen[1].1.as_deref(), Some("\"w1\""));
}

#[test]
#[serial]
fn an_inactive_mirror_falls_to_the_next_tier() {
    crate::forge_mirror::set_active(None);
    let mut d = Discovery::default();
    let snap = d
        .begin_tick(wl(), now())
        .with_lister(counting(Arc::default()));
    assert_eq!(snap.repo(&target("acme/widgets")).unwrap().tier, Tier::Gateway);
    assert_eq!(snap.stats().worklist_error_class, Some("transport"));
}

/// The snapshot answers `ReadKind::Discovery` only (`poll_gate` invariant 3):
/// no claim, label-transition or merge module names it, so no such decision
/// can read it. The reconcilers that will read it (#12307) join this list in
/// their own slices.
#[test]
fn only_the_step_tick_and_its_reporting_name_the_snapshot() {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut users = std::collections::BTreeSet::new();
    let mut stack = vec![src.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let rel = path
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            if rel.starts_with("discovery/") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let names = text.lines().any(|l| {
                let l = l.trim_start();
                !l.starts_with("//")
                    && (l.contains("crate::discovery") || l.contains("TickSnapshot"))
            });
            if names {
                users.insert(rel);
            }
        }
    }
    let expected: std::collections::BTreeSet<String> = [
        "observability/ops/dispatch_facts.rs",
        "step_dispatch/driver.rs",
        "step_dispatch/gather.rs",
        "work_finder.rs",
        "work_finder/tick_report.rs",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    assert_eq!(
        users, expected,
        "a new snapshot reader needs review against poll_gate invariant 3"
    );
}
