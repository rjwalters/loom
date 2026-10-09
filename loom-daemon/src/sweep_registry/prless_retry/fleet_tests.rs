//! Multi-host simulation of the PR-less retry bound (Issue #9292).
//!
//! [`super`]'s own test module drives ONE registry, which is the right shape
//! for everything about the tally that is local: the ladder, the streak-cold
//! rule, the fail-open probe arms. It cannot see the defect #9292 was filed
//! on, because that defect only exists when there is more than one counter.
//!
//! The observed shape, `rjwalters/loom#8812` on 2026-09-24: four dispatch
//! hosts, one failing issue, and four private tallies. Nine claim/release
//! cycles at roughly 90 s apart, four near-identical `Attempt 2 of 3` notes —
//! one per host — and only then a single host's third release reaching the
//! threshold and applying `loom:blocked`. Every host was individually correct;
//! the fleet spent `4 × threshold` claims to enforce a bound of `threshold`.
//!
//! So these tests build a *fleet*: N registries, each with its own peer-claim
//! view, wired through [`relay`] so one host's broadcast lands in every other
//! host's view under a distinct host identity — the job `safehouse`'s
//! `PeerClaimSink` does in production. Nothing here stubs the mechanism under
//! test; the registries record real releases, publish real
//! [`ClaimKind::PrlessReleaseArmed`] ads over a real channel, and read the
//! real peer view back.
//!
//! Lives in its own file rather than in `prless_retry.rs` because the harness
//! is most of its bulk and `.loom/docs/file-size-policy.md` prefers a sibling
//! to a growing parent.

use super::*;
use crate::peer_claims::{observe_brake_ad, ClaimAd, PeerClaimView};
use crate::sweep_registry::test_support::{
    fake_gh_graphql_arm, fake_gh_timeline_rest_arm, state_probe_json,
};
use crate::sweep_registry::{SweepRegistry, SweepRegistryConfig};
use serial_test::serial;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio::sync::mpsc::{channel, Receiver};

/// The repo slug every simulated host shares. Set into `LOOM_REPO` by
/// [`Fleet::new`] so all four registries key their ads identically — without
/// it each tempdir's basename would be its own repo and no ad would ever
/// match, which would make these tests pass for the wrong reason.
const FLEET_REPO: &str = "rjwalters/loom";

/// One simulated dispatch host.
struct Host {
    /// This host's cross-fleet identity, stamped onto every ad it publishes
    /// (see [`Fleet::relay`] for why it is rewritten rather than taken from
    /// `host_identity()`).
    name: String,
    reg: SweepRegistry,
    view: Arc<Mutex<PeerClaimView>>,
    rx: Receiver<ClaimAd>,
    _dir: TempDir,
}

/// A fleet of hosts sharing one logical repo and one peer-claim room.
struct Fleet {
    hosts: Vec<Host>,
    /// Every `gh` invocation any host made, one argv per line — the forge as
    /// the fleet sees it. `None` when the fleet was built with label flips
    /// disabled.
    gh_log: Option<PathBuf>,
}

impl Fleet {
    /// Build `n` hosts. `forge` selects whether label flips and comments are
    /// enabled: `false` gives the hermetic tally-only fixture
    /// (`skip_label_flip = true`, matching `prless_retry.rs`'s own
    /// `test_registry`), `true` points every host's `gh` at ONE shared fake
    /// that logs to ONE shared file — which is what makes "how many comments
    /// did the fleet post on this issue" a question a test can ask.
    fn new(n: usize, forge: bool) -> Self {
        std::env::set_var("LOOM_REPO", FLEET_REPO);
        // One shared workspace dir for the fake `gh` + log, so every host's
        // comments land in the same place the way they land on one issue.
        let shared = tempfile::tempdir().unwrap();
        let gh_log = shared.path().join("gh-invocations.log");
        let fake_gh = shared.path().join("fake-gh-fleet.sh");
        if forge {
            write_fake_gh(&fake_gh, &gh_log);
        }

        let mut hosts = Vec::with_capacity(n);
        for i in 0..n {
            let name = format!("host-{}", (b'a' + u8::try_from(i).unwrap()) as char);
            let dir = tempfile::tempdir().unwrap();
            let mut config = SweepRegistryConfig::new(dir.path().to_path_buf());
            config.skip_label_flip = !forge;
            if forge {
                config.gh_bin = Some(fake_gh.clone());
                config.journal_path = Some(dir.path().join("journal.json"));
            }
            let mut reg = SweepRegistry::new(config);

            // A view whose `self_host` is this host's name, so an ad relayed
            // to it from a *different* host is accepted and its own is not.
            let view =
                Arc::new(Mutex::new(PeerClaimView::new(name.clone(), Duration::from_secs(120))));
            let (tx, rx) = channel(64);
            reg.set_peer_claims(Arc::clone(&view));
            reg.set_peer_claim_publisher(tx);
            hosts.push(Host {
                name,
                reg,
                view,
                rx,
                _dir: dir,
            });
        }
        // `shared` must outlive the fake `gh` path every host holds; leak it
        // deliberately rather than thread another field through — a test
        // process exit reclaims it.
        let gh_log = forge.then(|| {
            let p = gh_log.clone();
            std::mem::forget(shared);
            p
        });
        Self { hosts, gh_log }
    }

    /// Drain every host's outbound channel and deliver each ad to every OTHER
    /// host's view — the `safehouse::PeerClaimSink` loop, minus the socket.
    ///
    /// The ad's `host` field is rewritten to the publishing host's simulated
    /// name because `publish_peer_prless_release_claim` stamps the real
    /// process's `host_identity()`, which is identical for all four simulated
    /// hosts here. Leaving it would collapse four `(repo, issue, host)` keys
    /// into one and silently restore the very per-host behaviour under test.
    fn relay(&mut self) {
        let mut pending: Vec<(usize, ClaimAd)> = Vec::new();
        for (i, host) in self.hosts.iter_mut().enumerate() {
            while let Ok(mut ad) = host.rx.try_recv() {
                // The socket layer routes on this predicate; assert the new
                // lane satisfies it rather than trusting the router alone.
                assert!(
                    ad.kind.is_cooldown_lane(),
                    "a PR-less release ad must ride the brake lane, got {:?}",
                    ad.kind
                );
                ad.host = host.name.clone();
                pending.push((i, ad));
            }
        }
        for (from, ad) in pending {
            for (j, host) in self.hosts.iter_mut().enumerate() {
                if j == from {
                    continue;
                }
                let mut view = host.view.lock().unwrap();
                observe_brake_ad(&mut view, &ad, Instant::now());
            }
        }
    }

    /// `host` records one PR-less release on `issue`, then the fleet catches
    /// up — one claim/release cycle exactly as the reaper drives it.
    fn release(&mut self, host: usize, issue: u32, reason: &str) {
        self.hosts[host].reg.record_prless_release(issue, reason);
        self.relay();
    }

    /// How many of this fleet's `gh issue comment` bodies carry `marker`.
    fn comments_with(&self, marker: &str) -> usize {
        let Some(log) = &self.gh_log else {
            return 0;
        };
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .matches(marker)
            .count()
    }
}

/// One fake `gh` shared by every host in a [`Fleet`]: logs every argv to
/// `gh_log`, answers "open issue, no open linked PR on either transport" so
/// neither hold veto fires, and exits 0 for the `issue edit` / `issue comment`
/// mutations these tests count. Arm order mirrors `hold.rs`'s
/// `forge_registry` — the REST timeline arm must precede the generic
/// `repos/*` state probe, whose glob would otherwise swallow it.
fn write_fake_gh(path: &Path, gh_log: &Path) {
    let script = format!(
        "#!/usr/bin/env bash\n\
         printf '%s\\n' \"$*\" >> \"{log}\"\n\
         {timeline}\
         {gql}\
         if [[ \"$1\" == \"api\" && \"$2\" == repos/* ]]; then\n\
         printf '%s\\n' '{state}'\n\
         exit 0\n\
         fi\n\
         if [[ \"$1\" == \"repo\" && \"$2\" == \"view\" ]]; then\n\
         printf '{repo}\\n'\n\
         exit 0\n\
         fi\n\
         exit 0\n",
        log = gh_log.display(),
        timeline = fake_gh_timeline_rest_arm("", 0),
        gql = fake_gh_graphql_arm("", 0),
        state = state_probe_json("open", false),
        repo = FLEET_REPO,
    );
    std::fs::write(path, &script).unwrap();
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).unwrap();
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.sync_all();
    }
}

/// #9518: a host whose identity fell through to `UNKNOWN_HOST` receives its own
/// echoed ads. They must not inflate the tally: counts stay 1, 2, 3 and the
/// hold trips on the third local release, not the second.
#[test]
#[serial]
fn echoed_unresolved_identity_ads_do_not_double_count() {
    let mut fleet = Fleet::new(1, false);
    fleet.hosts[0].view = Arc::new(Mutex::new(PeerClaimView::new(
        crate::sweep_registry::UNKNOWN_HOST.to_string(),
        Duration::from_secs(120),
    )));
    let view = Arc::clone(&fleet.hosts[0].view);
    fleet.hosts[0].reg.set_peer_claims(view);
    assert_eq!(fleet.hosts[0].reg.prless_retry_config().threshold, 3);

    for expected in 1..=3u32 {
        fleet.hosts[0].reg.record_prless_release(8812, "no PR");
        assert_eq!(fleet.hosts[0].reg.prless_fleet_release_count(8812), expected);
        // Echo the host's own ad back at it under the unresolved identity.
        while let Ok(mut ad) = fleet.hosts[0].rx.try_recv() {
            ad.host = crate::sweep_registry::UNKNOWN_HOST.to_string();
            let mut v = fleet.hosts[0].view.lock().unwrap();
            observe_brake_ad(&mut v, &ad, Instant::now());
        }
        assert_eq!(fleet.hosts[0].reg.prless_retry_held(8812), expected >= 3);
    }
}

// ======================================================================
// The headline regression: the threshold is fleet-wide, not per-host
// ======================================================================

/// #9292 AC1, and the whole point: four hosts each claiming the same failing
/// issue once must reach the hold on the **third** claim fleet-wide — not on
/// the twelfth, which is what four private tallies of three cost.
#[test]
#[serial]
fn four_hosts_trip_the_hold_at_the_threshold_not_at_four_times_it() {
    let mut fleet = Fleet::new(4, false);
    let threshold = fleet.hosts[0].reg.prless_retry_config().threshold;
    assert_eq!(threshold, DEFAULT_PRLESS_RETRY_THRESHOLD, "3 on shipped config");

    // Claim 1 — host A. Nobody holds; the fleet tally is 1.
    fleet.release(0, 8812, "builder crashed without opening a PR");
    assert_eq!(fleet.hosts[0].reg.prless_fleet_release_count(8812), 1);
    assert!(!fleet.hosts[0].reg.prless_retry_held(8812));

    // Claim 2 — host B, which has never seen this issue before and whose OWN
    // count is 1. Its fleet view is 2, so it is the host that posts the note.
    fleet.release(1, 8812, "builder crashed without opening a PR");
    assert_eq!(fleet.hosts[1].reg.prless_release_count(8812), 1, "B's own tally is 1");
    assert_eq!(
        fleet.hosts[1].reg.prless_fleet_release_count(8812),
        2,
        "…but the fleet has now spent two claims on this issue"
    );
    assert!(!fleet.hosts[1].reg.prless_retry_held(8812), "still one short");

    // Claim 3 — host C. Own count 1, fleet count 3: the threshold, reached on
    // the third claim the FLEET made rather than the third claim any one host
    // made. Pre-#9292 this was claim 3 of 12.
    fleet.release(2, 8812, "builder crashed without opening a PR");
    assert_eq!(fleet.hosts[2].reg.prless_release_count(8812), 1);
    assert_eq!(fleet.hosts[2].reg.prless_fleet_release_count(8812), 3);
    assert!(
        fleet.hosts[2].reg.prless_retry_held(8812),
        "the {threshold}rd claim fleet-wide must hold the issue (#9292)"
    );

    // And host D — which never dispatched this issue at all — already skips
    // it, because a peer's live window is unioned into its candidate filter.
    assert!(
        fleet.hosts[3]
            .reg
            .prless_retry_issues(Utc::now())
            .contains(&8812),
        "a host with no local record must still skip an issue its peers are braking"
    );
}

/// The counterfactual, run on the same harness: with the peer view detached
/// (`safehouse.enabled` false), the identical four-host sequence reproduces
/// the pre-#9292 behaviour exactly — nobody holds, because every host is
/// still counting alone. This is what pins the fix to the broadcast rather
/// than to some incidental change in the tally.
#[test]
#[serial]
fn without_peer_coordination_four_hosts_reproduce_the_per_host_tally() {
    std::env::set_var("LOOM_REPO", FLEET_REPO);
    let dirs: Vec<TempDir> = (0..4).map(|_| tempfile::tempdir().unwrap()).collect();
    let mut regs: Vec<SweepRegistry> = dirs
        .iter()
        .map(|d| {
            let mut config = SweepRegistryConfig::new(d.path().to_path_buf());
            config.skip_label_flip = true;
            SweepRegistry::new(config)
        })
        .collect();

    for reg in &mut regs {
        reg.record_prless_release(8812, "builder crashed without opening a PR");
    }

    for (i, reg) in regs.iter().enumerate() {
        assert_eq!(reg.prless_release_count(8812), 1, "host {i} counted only its own");
        assert_eq!(
            reg.prless_fleet_release_count(8812),
            1,
            "with no peer view the fleet count IS the local count (host {i})"
        );
        assert!(!reg.prless_retry_held(8812), "host {i} is nowhere near its private threshold");
    }
}

/// A single host's runway is unchanged: three of its own releases still hold
/// on the third, and its fleet count equals its local count throughout. The
/// #9292 acceptance criterion that the single-host case must not regress,
/// asserted on the fleet harness rather than only on the hermetic one.
#[test]
#[serial]
fn a_lone_host_in_a_wired_fleet_still_holds_on_its_own_third_release() {
    let mut fleet = Fleet::new(1, false);
    for attempt in 1..=3u32 {
        fleet.release(0, 8812, "no PR");
        assert_eq!(fleet.hosts[0].reg.prless_release_count(8812), attempt);
        assert_eq!(fleet.hosts[0].reg.prless_fleet_release_count(8812), attempt);
    }
    assert!(fleet.hosts[0].reg.prless_retry_held(8812));
}

/// A host whose own streak went cold, dispatching an issue its peers are
/// actively failing, must not be handed a fresh full runway: the peer term is
/// what it inherits. This is the fleet-wide form of the streak-cold rule.
#[test]
#[serial]
fn a_fresh_host_joining_a_live_streak_inherits_the_fleet_count() {
    let mut fleet = Fleet::new(3, false);
    fleet.release(0, 8812, "first");
    fleet.release(0, 8812, "second");
    assert_eq!(fleet.hosts[0].reg.prless_release_count(8812), 2);

    // Host C has never touched this issue. Its very first release is the
    // fleet's third.
    fleet.release(2, 8812, "third, from a host that had never claimed it");
    assert_eq!(fleet.hosts[2].reg.prless_release_count(8812), 1);
    assert_eq!(fleet.hosts[2].reg.prless_fleet_release_count(8812), 3);
    assert!(fleet.hosts[2].reg.prless_retry_held(8812));
}

/// Peer tallies lapse. A crashed peer — one that broadcast a streak and then
/// stopped — must stop contributing once its advertised window elapses,
/// rather than pinning the issue one release short of `loom:blocked` forever.
#[test]
#[serial]
fn a_lapsed_peer_tally_stops_counting_toward_the_threshold() {
    let mut fleet = Fleet::new(2, false);
    fleet.release(0, 8812, "peer A's streak");
    fleet.release(0, 8812, "peer A's streak");

    let view = Arc::clone(&fleet.hosts[1].view);
    let now = Instant::now();
    assert_eq!(
        view.lock()
            .unwrap()
            .prless_peer_release_count_at(FLEET_REPO, 8812, now),
        2
    );

    // Well past the advertised window (the ladder's second step is 600 s).
    let later = now + Duration::from_secs(4 * 3600);
    assert_eq!(
        view.lock()
            .unwrap()
            .prless_peer_release_count_at(FLEET_REPO, 8812, later),
        0,
        "a peer that stopped advertising stops counting"
    );
    view.lock().unwrap().prune_expired_prless_releases(later);
    assert!(view
        .lock()
        .unwrap()
        .prless_release_issues_at(FLEET_REPO, later)
        .is_empty());
}

// ======================================================================
// Comment volume: one note per streak per ISSUE, not per host
// ======================================================================

/// #9292 AC2: the four near-identical `Attempt 2 of 3` notes on
/// `rjwalters/loom#8812` — one per dispatch host, 24 minutes apart, none of
/// them a hold — must become exactly one. The fleet's `gh` calls all land in
/// one log here, so this counts the comments an issue would actually receive.
#[test]
#[serial]
fn a_four_host_streak_posts_exactly_one_attempt_note() {
    let mut fleet = Fleet::new(4, true);

    // Four hosts, four claims, in the interleaved order the trace shows.
    for host in 0..4 {
        fleet.release(host, 8812, "builder crashed without opening a PR");
    }

    assert_eq!(
        fleet.comments_with(PRLESS_ATTEMPT_COMMENT_MARKER),
        1,
        "one attempt note per streak per ISSUE, not one per host (#9292)"
    );
    assert!(
        fleet.comments_with(PRLESS_HOLD_COMMENT_MARKER) >= 1,
        "the streak still reaches a real hold"
    );
}

/// The mixed shape, and the reason the ad carries a **count** rather than just
/// its window: one host releases twice (posting the note at fleet 2), then a
/// second host releases once. If a peer entry counted as a flat "this host saw
/// something" the second host would compute 1 + 1 = 2 and post a duplicate note
/// while the fleet had in fact spent three claims. With the real count on the
/// wire it computes 1 + 2 = 3, holds, and posts no second note.
#[test]
#[serial]
fn a_host_with_a_streak_of_two_is_counted_as_two_by_its_peers() {
    let mut fleet = Fleet::new(2, true);
    fleet.release(0, 8812, "first");
    fleet.release(0, 8812, "second");
    assert_eq!(fleet.hosts[0].reg.prless_fleet_release_count(8812), 2);
    assert_eq!(
        fleet.comments_with(PRLESS_ATTEMPT_COMMENT_MARKER),
        1,
        "host A posted the streak's one note at fleet 2"
    );

    fleet.release(1, 8812, "third, from the second host");

    assert_eq!(
        fleet.hosts[1].reg.prless_release_count(8812),
        1,
        "host B's own tally is one release"
    );
    assert_eq!(
        fleet.hosts[1].reg.prless_fleet_release_count(8812),
        3,
        "…but it must inherit host A's TWO, not a flat one-per-peer"
    );
    assert!(fleet.hosts[1].reg.prless_retry_held(8812));
    assert_eq!(
        fleet.comments_with(PRLESS_ATTEMPT_COMMENT_MARKER),
        1,
        "still exactly one attempt note on the issue (#9292 AC2)"
    );
}

/// The same run, from the other side: the note the fleet does post is the
/// `Attempt 2 of 3` one, posted by whichever host made the fleet's SECOND
/// claim — and the hold that follows names the fleet total, not that host's
/// private count of one.
#[test]
#[serial]
fn the_single_note_and_the_hold_both_name_the_fleet_count() {
    let mut fleet = Fleet::new(4, true);
    for host in 0..3 {
        fleet.release(host, 8812, "scope `safehouse_chatops/` does not exist on main");
    }

    let log = std::fs::read_to_string(fleet.gh_log.as_ref().unwrap()).unwrap();
    assert!(
        log.contains("**Attempt 2 of 3 ended without a pull request.**"),
        "the one note names where in the FLEET's runway the streak sits: {log}"
    );
    assert!(
        log.contains("**Held after 3 consecutive claims that produced no pull request.**"),
        "the hold names the fleet total, though no single host claimed three times: {log}"
    );
}

/// The label write is still the deliverable (#9239), and it happens **once**:
/// three hosts between them produce exactly one `gh issue edit` applying
/// `loom:blocked`, from the host whose release crossed the threshold.
#[test]
#[serial]
fn only_the_threshold_crossing_host_flips_the_label() {
    let mut fleet = Fleet::new(4, true);
    for host in 0..3 {
        fleet.release(host, 8812, "builder crashed without opening a PR");
    }

    let edits: Vec<String> = std::fs::read_to_string(fleet.gh_log.as_ref().unwrap())
        .unwrap()
        .lines()
        .filter(|l| l.starts_with("issue edit "))
        .map(std::string::ToString::to_string)
        .collect();
    assert_eq!(edits.len(), 1, "exactly one label flip across the fleet, got: {edits:?}");
    // `--repo` rides along because `Fleet::new` sets `LOOM_REPO` to give
    // every simulated host one shared repo slug.
    assert_eq!(
        edits[0],
        format!("issue edit 8812 --add-label loom:blocked --remove-label loom:issue --repo {FLEET_REPO}")
    );
}
