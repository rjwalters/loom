//! Tests for `loom-daemon lease pr` (#9401).
//!
//! The ordering, yield and fence decisions are pure and exercised directly;
//! `claim` is then driven end to end against a FAKE `gh` that keeps a
//! comment list on disk, so the pre-check, the post, the read-back and the
//! yield record are observed exactly as a claimant sees them.

use super::renew::{
    decide, forge_guard, forge_state, pre_patch_guard, renew_margin_secs, wall_secs, Clocks, Step,
    LABEL_MISS_LIMIT, RENEW_MARGIN_SECS,
};
use super::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use tempfile::TempDir;

use loom_daemon::forge_identity::{FleetLogins, Roster};

fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-11T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn ttl() -> chrono::Duration {
    chrono::Duration::minutes(15)
}

fn who(role: Role, sweep: &str) -> Identity {
    Identity {
        role,
        host: "host-a1b2c3d4".into(),
        sweep: sweep.into(),
        step: None,
    }
}

fn row(id: u64, age_min: i64, body: String) -> Row {
    Row {
        id,
        updated_at: now() - chrono::Duration::minutes(age_min),
        body,
    }
}

fn lease(id: u64, age_min: i64, holder: &Identity) -> Row {
    row(id, age_min, format!("{}\nprose", holder.lease_marker()))
}

fn state(rows: &[Row], me: &Identity) -> ClaimState {
    claim_state(Some(rows), me, now(), ttl())
}

// ---- markers ------------------------------------------------------------

#[test]
fn markers_round_trip_and_only_the_first_line_is_read() {
    let a = who(Role::Doctor, "sweep-a");
    let b = who(Role::Judge, "sweep-b");
    assert_eq!(parse_record(&a.lease_marker()), Some(Record::Lease(a.clone())));
    assert_eq!(parse_record(&a.released_marker()), Some(Record::Released(a.clone())));
    assert_eq!(parse_record(&a.yield_marker(&b)), Some(Record::Yield(a.clone())));
    let spoof = format!("prose first\n{}", a.lease_marker());
    assert_eq!(parse_record(&spoof), None, "a marker below line one is prose");
}

#[test]
fn malformed_markers_are_not_records() {
    for body in [
        "<!-- loom:pr-lease role=builder host=h sweep=s -->",
        "<!-- loom:pr-lease role=doctor host= sweep=s -->",
        "<!-- loom:pr-lease role=doctor sweep=s host=h -->",
        "<!-- loom:pr-lease role=doctor host=h sweep=s",
    ] {
        assert_eq!(parse_record(body), None, "{body}");
    }
}

#[test]
fn issue_leases_and_pr_leases_never_read_as_each_other() {
    // The issue-path readers match `<!-- loom:lease host=`; a PR lease must
    // not, and an issue lease must not read as a PR lease.
    let pr = who(Role::Doctor, "s").lease_marker();
    assert!(!pr.starts_with(loom_daemon::sweep_registry::LEASE_MARKER_PREFIX));
    assert!(!pr.starts_with("<!-- loom:lease-yield host="));
    assert_eq!(parse_record("<!-- loom:lease host=h sweep=s -->"), None);
}

// ---- ordering: earliest live lease of the role wins ---------------------

#[test]
fn of_two_live_leases_the_earlier_comment_id_wins() {
    let a = who(Role::Doctor, "sweep-a");
    let b = who(Role::Doctor, "sweep-b");
    // B renewed more recently; order is still comment id, never updated_at.
    let rows = [lease(10, 4, &a), lease(20, 0, &b)];
    assert_eq!(state(&rows, &a), ClaimState::HeldBySelf { id: 10 });
    assert_eq!(
        state(&rows, &b),
        ClaimState::HeldByPeer {
            id: 10,
            holder: a.clone()
        }
    );
    assert_eq!(ordering(&state(&rows, &b)), Ordering::Lost(a));
}

#[test]
fn doctor_and_judge_leases_do_not_contend() {
    let doctor = who(Role::Doctor, "sweep-a");
    let judge = who(Role::Judge, "sweep-b");
    let rows = [lease(10, 1, &doctor), lease(20, 0, &judge)];
    assert_eq!(state(&rows, &judge), ClaimState::HeldBySelf { id: 20 });
    assert_eq!(state(&rows, &doctor), ClaimState::HeldBySelf { id: 10 });
    // Same sweep and host, different roles: still two separate claims.
    let judge_same_sweep = who(Role::Judge, "sweep-a");
    assert_eq!(state(&rows[..1], &judge_same_sweep), ClaimState::Free);
}

#[test]
fn a_stale_lease_outside_the_ttl_does_not_block() {
    let dead = who(Role::Doctor, "sweep-dead");
    let me = who(Role::Doctor, "sweep-me");
    assert_eq!(state(&[lease(10, 20, &dead)], &me), ClaimState::Free);
    let rows = [lease(10, 20, &dead), lease(20, 0, &me)];
    assert_eq!(state(&rows, &me), ClaimState::HeldBySelf { id: 20 });
    // Exactly at the TTL is no longer live.
    assert_eq!(state(&[lease(10, 15, &dead)], &me), ClaimState::Free);
    assert!(matches!(state(&[lease(10, 14, &dead)], &me), ClaimState::HeldByPeer { .. }));
}

#[test]
fn a_yielded_lease_does_not_block_but_an_older_yield_does_not_bury_a_new_claim() {
    let loser = who(Role::Doctor, "sweep-l");
    let me = who(Role::Doctor, "sweep-me");
    let winner = who(Role::Doctor, "sweep-w");
    let yielded = row(30, 0, loser.yield_marker(&winner));
    let rows = [lease(10, 0, &loser), lease(20, 0, &me), yielded.clone()];
    assert_eq!(state(&rows, &me), ClaimState::HeldBySelf { id: 20 });
    // A yield that predates the lease is from an earlier episode.
    let rows = [row(5, 0, loser.yield_marker(&winner)), lease(10, 0, &loser)];
    assert!(matches!(state(&rows, &me), ClaimState::HeldByPeer { id: 10, .. }));
}

#[test]
fn a_released_lease_frees_the_claim_at_once() {
    let done = who(Role::Judge, "sweep-done");
    let me = who(Role::Judge, "sweep-me");
    assert_eq!(state(&[row(10, 0, done.released_marker())], &me), ClaimState::Free);
}

#[test]
fn an_unreadable_list_is_unknown_never_free() {
    let me = who(Role::Doctor, "s");
    assert_eq!(claim_state(None, &me, now(), ttl()), ClaimState::Unknown);
    assert_eq!(ordering(&ClaimState::Unknown), Ordering::Unverified);
    assert_eq!(ordering(&ClaimState::Free), Ordering::Unverified);
}

// ---- fence --------------------------------------------------------------

#[test]
fn fence_passes_the_holder_and_stops_everyone_else() {
    let a = who(Role::Doctor, "sweep-a");
    let b = who(Role::Doctor, "sweep-b");
    let rows = [lease(10, 0, &a), lease(20, 0, &b)];
    assert_eq!(fence(Some(&rows), &a, now(), ttl()), Fence::Pass);
    assert_eq!(fence(Some(&rows), &b, now(), ttl()), Fence::Superseded(a.clone()));
    // Own lease aged out with no peer: expired, not a pass.
    assert_eq!(fence(Some(&[lease(10, 30, &a)]), &a, now(), ttl()), Fence::Expired);
    // Never leased (an older prompt) or unreadable: no evidence, proceed.
    assert_eq!(fence(Some(&[]), &a, now(), ttl()), Fence::NoEvidence);
    assert_eq!(fence(None, &a, now(), ttl()), Fence::NoEvidence);
    // A Judge's live lease never fences a Doctor.
    let judge = who(Role::Judge, "sweep-j");
    assert_eq!(
        fence(Some(&[lease(5, 0, &judge), lease(10, 0, &a)]), &a, now(), ttl()),
        Fence::Pass
    );
}

// ---- identity -----------------------------------------------------------

#[test]
fn sweep_identity_precedence() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |k: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| (*v).to_string())
        }
    };
    assert_eq!(
        resolve_sweep(Some("explicit"), None, env(&[("LOOM_SWEEP_RUN_ID", "r")])),
        "explicit"
    );
    assert_eq!(
        resolve_sweep(
            None,
            None,
            env(&[("LOOM_SWEEP_RUN_ID", "r"), ("LOOM_TERMINAL_ID", "daemon-d")])
        ),
        "r"
    );
    assert_eq!(resolve_sweep(None, None, env(&[("LOOM_TERMINAL_ID", "daemon-d")])), "d");
    assert_eq!(
        resolve_sweep(None, None, env(&[("LOOM_TERMINAL_ID", "terminal-3")])),
        "pr-lease-unknown"
    );
    // A live pid gives the stable attended id `lease ensure` uses.
    let me = std::process::id();
    assert!(resolve_sweep(None, Some(me), env(&[])).starts_with(&format!("sweep-insession-s{me}-")));
}

#[test]
fn the_lease_body_starts_with_its_marker_and_a_renewal_changes_it() {
    let me = who(Role::Doctor, "s");
    let first = lease_body(&me, 7, None, "<!-- loom:provenance v1 x -->");
    assert_eq!(first.lines().next(), Some(me.lease_marker().as_str()));
    assert!(first.ends_with("<!-- loom:provenance v1 x -->"));
    let renewed = lease_body(&me, 7, Some(now()), "");
    assert_ne!(first, renewed);
    assert_eq!(parse_record(&renewed), Some(Record::Lease(me)));
}

// ---- renewer ------------------------------------------------------------

#[test]
fn the_renewer_stops_and_skips_on_the_right_signals() {
    let open = (true, vec!["loom:treating".to_string()]);
    let bare = (true, Vec::new());
    let closed = (false, vec!["loom:treating".to_string()]);
    let ok = lease_renewer_exit_renew();
    let mut m = 0;
    assert_eq!(
        decide(true, fresh(10, 100), ok, Some(&open), Some("loom:treating"), &mut m),
        Step::Renew
    );
    assert!(matches!(
        decide(false, fresh(10, 100), ok, Some(&open), Some("loom:treating"), &mut m),
        Step::Stop(_)
    ));
    assert!(matches!(
        decide(true, fresh(100, 100), ok, Some(&open), Some("loom:treating"), &mut m),
        Step::Stop(_)
    ));
    assert!(matches!(
        decide(true, fresh(10, 100), 3, Some(&open), Some("loom:treating"), &mut m),
        Step::Stop(_)
    ));
    assert!(matches!(
        decide(true, fresh(10, 100), ok, Some(&closed), Some("loom:treating"), &mut m),
        Step::Stop(_)
    ));
    assert_eq!(
        decide(true, fresh(10, 100), ok, None, Some("loom:treating"), &mut m),
        Step::Skip
    );
    // The label must be gone for LABEL_MISS_LIMIT consecutive cycles.
    m = 0;
    for _ in 1..LABEL_MISS_LIMIT {
        assert_eq!(
            decide(true, fresh(10, 100), ok, Some(&bare), Some("loom:treating"), &mut m),
            Step::Skip
        );
    }
    assert!(matches!(
        decide(true, fresh(10, 100), ok, Some(&bare), Some("loom:treating"), &mut m),
        Step::Stop(_)
    ));
    m = 1;
    assert_eq!(
        decide(true, fresh(10, 100), ok, Some(&open), Some("loom:treating"), &mut m),
        Step::Renew
    );
    assert_eq!(m, 0, "a present label resets the streak");
}

/// Clocks for a lease renewed a minute ago, `elapsed` into a `cap` lifetime.
fn fresh(elapsed: i64, cap: u64) -> Clocks {
    Clocks {
        elapsed_secs: elapsed,
        cap_secs: cap,
        since_renewal_secs: 60,
        ttl_secs: 900,
    }
}

#[test]
fn the_renewer_stops_once_its_last_renewal_is_a_ttl_old_by_the_wall_clock() {
    let open = (true, vec!["loom:treating".to_string()]);
    let ok = lease_renewer_exit_renew();
    let mut m = 0;
    let at = |since: i64| Clocks {
        since_renewal_secs: since,
        ..fresh(10, 14_400)
    };
    // Inside the TTL less the margin: renew.
    let limit = i64::try_from(900 - RENEW_MARGIN_SECS).unwrap();
    assert_eq!(
        decide(true, at(limit - 1), ok, Some(&open), Some("loom:treating"), &mut m),
        Step::Renew
    );
    // At or past it, even with the PR open and labelled and the forge
    // unreadable: stop, never renew.
    for since in [limit, 900, 20 * 60, 3 * 3600] {
        assert!(
            matches!(
                decide(true, at(since), ok, None, Some("loom:treating"), &mut m),
                Step::Stop(_)
            ),
            "{since}"
        );
        assert!(matches!(
            decide(true, at(since), ok, Some(&open), Some("loom:treating"), &mut m),
            Step::Stop(_)
        ));
    }
    // A short TTL clamps the margin to a quarter of it.
    let short = Clocks {
        since_renewal_secs: 44,
        ttl_secs: 60,
        ..fresh(10, 14_400)
    };
    assert_eq!(decide(true, short, ok, Some(&open), Some("loom:treating"), &mut m), Step::Renew);
    let short = Clocks {
        since_renewal_secs: 45,
        ..short
    };
    assert!(matches!(
        decide(true, short, ok, Some(&open), Some("loom:treating"), &mut m),
        Step::Stop(_)
    ));
}

#[test]
fn the_age_cap_and_the_renewal_age_are_wall_clock() {
    // A Mac asleep 5 h: `Instant` would read ~0 elapsed; the wall clock
    // reads 5 h, past both the cap and the TTL.
    let started = now() - chrono::Duration::hours(5);
    let woke = now();
    let clocks = Clocks {
        elapsed_secs: wall_secs(woke, started),
        cap_secs: 14_400,
        since_renewal_secs: wall_secs(woke, started),
        ttl_secs: 900,
    };
    let open = (true, vec!["loom:reviewing".to_string()]);
    let mut m = 0;
    assert_eq!(
        decide(
            true,
            clocks,
            lease_renewer_exit_renew(),
            Some(&open),
            Some("loom:reviewing"),
            &mut m
        ),
        Step::Stop("the age cap passed")
    );
    assert_eq!(wall_secs(woke, started), 5 * 3600);
}

#[test]
fn a_woken_renewer_never_revives_an_expired_lease_a_peer_has_since_claimed() {
    // A's lease (comment 100) went 20 min without a renewal (host asleep, or
    // three unreadable cycles); it aged out and peer B claimed (comment 200).
    let a = who(Role::Doctor, "sweep-a");
    let b = who(Role::Doctor, "sweep-b");
    let rows = vec![lease(100, 20, &a), lease(200, 1, &b)];
    assert_eq!(state(&rows, &b), ClaimState::HeldBySelf { id: 200 });
    // A's renewer wakes. The wall-clock guard stops it before any PATCH…
    let open = (true, vec!["loom:treating".to_string()]);
    let woke = Clocks {
        since_renewal_secs: 20 * 60,
        ..fresh(30 * 60, 14_400)
    };
    let mut m = 0;
    assert!(matches!(
        decide(
            true,
            woke,
            lease_renewer_exit_renew(),
            Some(&open),
            Some("loom:treating"),
            &mut m
        ),
        Step::Stop(_)
    ));
    // …and the forge guard stops it independently: B holds the claim.
    let seen_by_a = state(&rows, &a);
    assert!(matches!(seen_by_a, ClaimState::HeldByPeer { id: 200, .. }));
    assert!(matches!(forge_guard(Some(&seen_by_a), 100), Step::Stop(_)));
    // With no peer yet, A's aged-out lease reads Free: still no renewal.
    let alone = vec![lease(100, 20, &a)];
    assert!(matches!(forge_guard(Some(&state(&alone, &a)), 100), Step::Stop(_)));
    // Nothing was renewed, so B's fence still passes and A's refuses.
    assert_eq!(fence(Some(&rows), &b, now(), ttl()), Fence::Pass);
    assert_eq!(fence(Some(&rows), &a, now(), ttl()), Fence::Superseded(b));
}

#[test]
fn the_forge_guard_renews_only_the_holders_own_earliest_live_lease() {
    let a = who(Role::Judge, "sweep-a");
    let rows = vec![lease(100, 4, &a)];
    assert_eq!(forge_guard(Some(&state(&rows, &a)), 100), Step::Renew);
    // A different comment of the same identity is the earliest: not ours.
    assert!(matches!(forge_guard(Some(&state(&rows, &a)), 300), Step::Stop(_)));
    // Unreadable: defer to the wall-clock guard `decide` already applied.
    assert_eq!(forge_guard(None, 100), Step::Renew);
    assert_eq!(forge_guard(Some(&ClaimState::Unknown), 100), Step::Renew);
}

#[test]
fn the_margin_outlasts_the_gh_calls_after_the_clock_check() {
    // The forge read and the PATCH, each bounded by GH_TIMEOUT, plus slack.
    assert!(RENEW_MARGIN_SECS > 2 * GH_TIMEOUT.as_secs());
    // At the default TTL the clamp does not bite.
    assert_eq!(renew_margin_secs(900), RENEW_MARGIN_SECS);
    assert_eq!(renew_margin_secs(60), 15);
}

#[test]
fn the_wall_clock_is_rechecked_immediately_before_the_patch() {
    // The top-of-cycle check passes one second inside the margin…
    let open = (true, vec!["loom:treating".to_string()]);
    let limit = i64::try_from(900 - RENEW_MARGIN_SECS).unwrap();
    let last_renewed = now() - chrono::Duration::seconds(limit - 1);
    let checked = Clocks {
        since_renewal_secs: wall_secs(now(), last_renewed),
        ..fresh(10, 14_400)
    };
    let mut m = 0;
    assert_eq!(
        decide(
            true,
            checked,
            lease_renewer_exit_renew(),
            Some(&open),
            Some("loom:treating"),
            &mut m
        ),
        Step::Renew
    );
    assert_eq!(pre_patch_guard(now(), last_renewed, 900), Step::Renew);
    // …then a slow forge read (up to GH_TIMEOUT) carries it into the margin:
    // the pre-PATCH re-check stops the loop rather than PATCH.
    let after_read = now() + chrono::Duration::from_std(GH_TIMEOUT).unwrap();
    assert!(matches!(pre_patch_guard(after_read, last_renewed, 900), Step::Stop(_)));
    // One second is enough to cross it.
    assert!(matches!(
        pre_patch_guard(now() + chrono::Duration::seconds(1), last_renewed, 900),
        Step::Stop(_)
    ));
}

#[test]
fn the_forge_guard_judges_its_own_lease_against_ttl_less_the_margin() {
    let a = who(Role::Doctor, "sweep-a");
    let b = who(Role::Doctor, "sweep-b");
    // Live against the full TTL (13 min < 15) but inside the margin
    // (13 min > 15 min - RENEW_MARGIN_SECS): no renewal.
    let late = vec![lease(100, 13, &a)];
    assert_eq!(state(&late, &a), ClaimState::HeldBySelf { id: 100 });
    assert_eq!(forge_state(&late, &a, now(), ttl()), ClaimState::Free);
    assert!(matches!(
        forge_guard(Some(&forge_state(&late, &a, now(), ttl())), 100),
        Step::Stop(_)
    ));
    // Comfortably live: renew.
    let ok = vec![lease(100, 5, &a)];
    assert_eq!(forge_guard(Some(&forge_state(&ok, &a, now(), ttl())), 100), Step::Renew);
    // A peer's older lease inside the margin is still judged at the full
    // TTL, as its own fence judges it: the peer holds, A must not renew.
    let peer = vec![lease(100, 13, &b), lease(200, 1, &a)];
    assert!(matches!(
        forge_state(&peer, &a, now(), ttl()),
        ClaimState::HeldByPeer { id: 100, .. }
    ));
    assert!(matches!(
        forge_guard(Some(&forge_state(&peer, &a, now(), ttl())), 200),
        Step::Stop(_)
    ));
}

fn lease_renewer_exit_renew() -> i32 {
    super::super::lease_renewer::EXIT_RENEW
}

// ---- claim end to end, against a fake gh --------------------------------

const AUTHOR: &str =
    r#""user":{"login":"loom-fleet-dispatch[bot]","type":"Bot"},"author_association":"NONE""#;

/// A fake `gh` keeping the PR's comments in `<dir>/rows` (NDJSON). A POST
/// appends a row holding the body's marker line, and a PATCH replaces that
/// comment's row (marker line, `updated_at` now); rows in `<dir>/late` join
/// the list after the first comments read (a peer racing the pre-check).
/// `<dir>/pull` answers the PR read (already in its `--jq` shape) and
/// `<dir>/checks` the head's check runs; without them both are unreadable.
/// Every call runs under a lock, so claimants on several threads see one
/// consistent comment list, as they would on the forge.
fn fake_gh(dir: &Path) -> PathBuf {
    let d = dir.display();
    let script = format!(
        r#"#!/usr/bin/env bash
D="{d}"
[ -f "$D/fail" ] && {{ echo 'HTTP 502' >&2; exit 1; }}
until mkdir "$D/lock" 2>/dev/null; do sleep 0.01; done
trap 'rmdir "$D/lock"' EXIT
case " $* " in
  *" POST "*)
    prev=""; body=""
    for a in "$@"; do [ "$prev" = "-f" ] && body="${{a#body=}}"; prev="$a"; done
    first="${{body%%$'\n'*}}"
    id=$(cat "$D/next"); echo $((id + 1)) > "$D/next"
    printf '{{"id":%s,"updated_at":"%s","body":"%s",%s}}\n' "$id" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$first" '{AUTHOR}' >> "$D/rows"
    echo "$first" >> "$D/posted"
    echo "$id" ;;
  *" PATCH "*)
    echo "$*" >> "$D/patched"
    prev=""; body=""; cid=""
    for a in "$@"; do
      [ "$prev" = "-f" ] && body="${{a#body=}}"
      case "$a" in */issues/comments/*) cid="${{a##*/}}" ;; esac
      prev="$a"
    done
    first="${{body%%$'\n'*}}"
    grep -v "^{{\"id\":$cid," "$D/rows" > "$D/rows.tmp" || true
    printf '{{"id":%s,"updated_at":"%s","body":"%s",%s}}\n' "$cid" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$first" '{AUTHOR}' >> "$D/rows.tmp"
    mv "$D/rows.tmp" "$D/rows" ;;
  *"/check-runs"*) [ -f "$D/checks" ] || exit 1; cat "$D/checks" ;;
  *"/pulls/"*) echo "$*" >> "$D/pulled"; cat "$D/pull" 2>/dev/null || echo "" ;;
  *"/comments"*)
    n=$(cat "$D/reads" 2>/dev/null || echo 0); echo $((n + 1)) > "$D/reads"
    if [ "$n" -ge 1 ] && [ -f "$D/late" ]; then cat "$D/late" >> "$D/rows"; rm "$D/late"; fi
    cat "$D/rows" ;;
  *) exit 1 ;;
esac
"#
    );
    let path = dir.join("gh");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(dir.join("rows"), "").unwrap();
    fs::write(dir.join("next"), "500").unwrap();
    path
}

fn live_row(id: u64, holder: &Identity) -> String {
    let at = Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    format!(
        r#"{{"id":{id},"updated_at":"{at}","body":"{}",{AUTHOR}}}"#,
        holder.lease_marker()
    ) + "\n"
}

fn ctx(dir: &Path, me: Identity) -> Ctx {
    Ctx {
        common: Common {
            pr: 77,
            role: me.role,
            step: me.step,
            watch_pid: None,
            sweep_id: Some(me.sweep.clone()),
            workspace: dir.to_path_buf(),
        },
        me,
        gh: Gh {
            bin: fake_gh_path(dir),
            root: dir.to_path_buf(),
            write_repo: Ok("acme/widgets".to_string()),
        },
        policy: TrustPolicy::new(FleetLogins::of(&Roster::default()), None, Vec::new()),
        ttl: ttl(),
        now: Utc::now,
        manage_renewer: false,
        lease_file: None,
    }
}

fn fake_gh_path(dir: &Path) -> PathBuf {
    dir.join("gh")
}

fn posted(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("posted"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn claim_first_claimant_wins_and_a_second_stands_down_without_writing() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    let a = who(Role::Doctor, "sweep-a");
    assert_eq!(ctx(dir.path(), a.clone()).claim(), EXIT_HELD);
    assert_eq!(posted(dir.path()), vec![a.lease_marker()]);
    let b = who(Role::Doctor, "sweep-b");
    assert_eq!(ctx(dir.path(), b).claim(), EXIT_PEER);
    assert_eq!(posted(dir.path()).len(), 1, "a peer-held claim writes nothing");
    // Re-claiming as the holder re-attaches instead of posting again.
    assert_eq!(ctx(dir.path(), a.clone()).claim(), EXIT_HELD);
    assert_eq!(posted(dir.path()).len(), 1);
    assert_eq!(ctx(dir.path(), a).fence(), EXIT_HELD);
}

#[test]
fn claim_that_loses_the_race_posts_a_yield_and_exits_peer() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    // A peer's lease with an EARLIER id lands after our pre-check read.
    let peer = who(Role::Doctor, "sweep-peer");
    fs::write(dir.path().join("late"), live_row(100, &peer)).unwrap();
    let me = who(Role::Doctor, "sweep-me");
    assert_eq!(ctx(dir.path(), me.clone()).claim(), EXIT_PEER);
    let posted = posted(dir.path());
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert_eq!(posted[0], me.lease_marker());
    assert_eq!(posted[1], me.yield_marker(&peer));
    // The loser's own fence now refuses too, and the winner's passes.
    assert_eq!(ctx(dir.path(), me).fence(), EXIT_PEER);
    assert_eq!(ctx(dir.path(), peer).fence(), EXIT_HELD);
}

#[test]
fn claim_refuses_without_writing_when_the_list_is_unreadable() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    fs::write(dir.path().join("fail"), "").unwrap();
    assert_eq!(ctx(dir.path(), who(Role::Judge, "s")).claim(), EXIT_UNVERIFIED);
    assert!(posted(dir.path()).is_empty());
}

#[test]
fn a_judge_claims_while_a_doctor_holds_the_pr() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    fs::write(dir.path().join("rows"), live_row(100, &who(Role::Doctor, "sweep-d"))).unwrap();
    assert_eq!(ctx(dir.path(), who(Role::Judge, "sweep-j")).claim(), EXIT_HELD);
    assert_eq!(ctx(dir.path(), who(Role::Doctor, "sweep-x")).check(), EXIT_PEER);
}

#[test]
fn release_rewrites_the_marker_so_the_claim_frees_at_once() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    let a = who(Role::Doctor, "sweep-a");
    fs::write(dir.path().join("rows"), live_row(100, &a)).unwrap();
    assert_eq!(ctx(dir.path(), a.clone()).release(None), EXIT_HELD);
    let patched = fs::read_to_string(dir.path().join("patched")).unwrap();
    assert!(patched.contains("repos/acme/widgets/issues/comments/100"), "{patched}");
    assert!(patched.contains(&a.released_marker()), "{patched}");
}

#[test]
fn claim_refuses_without_writing_when_write_scope_denies_the_repo() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    let mut c = ctx(dir.path(), who(Role::Doctor, "s"));
    c.gh.write_repo = Err("not a managed repository".to_string());
    assert_eq!(c.claim(), EXIT_UNVERIFIED);
    assert!(posted(dir.path()).is_empty(), "#9548: no write to an unvetted repo");
}

// ---- a timed-out Judge, reaped by the daemon (#11931) ---------------------

const HEAD: &str = "3333333333333333333333333333333333333333";

fn on(host: &str, sweep: &str) -> Identity {
    Identity {
        role: Role::Judge,
        host: host.into(),
        sweep: sweep.into(),
        step: None,
    }
}

/// The PR read the fake answers: open, waiting for review, claimed.
fn waiting_pr(dir: &Path) {
    let pull = format!(
        r#"{{"state":"open","head":"{HEAD}","labels":["loom:review-requested","loom:reviewing"]}}"#
    );
    fs::write(dir.join("pull"), pull).unwrap();
}

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn row_at(id: u64, at: DateTime<Utc>, first_line: &str) -> String {
    format!(r#"{{"id":{id},"updated_at":"{}","body":"{first_line}",{AUTHOR}}}"#, stamp(at)) + "\n"
}

fn patched(dir: &Path) -> String {
    fs::read_to_string(dir.join("patched")).unwrap_or_default()
}

#[test]
fn a_timed_out_judge_is_reaped_into_a_record_that_holds_the_pr_on_every_host() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    waiting_pr(dir.path());
    // The head's CI is still running, young: a release by the Judge itself
    // would not be recorded, a reap after a timeout is.
    let young = stamp(Utc::now() - chrono::Duration::minutes(10));
    fs::write(
        dir.path().join("checks"),
        format!(r#"{{"total_count":1,"check_runs":[{{"status":"in_progress","started_at":"{young}"}}]}}"#),
    )
    .unwrap();
    let dead = on("host-a", "run-1");
    let lease_file = dir.path().join("run-1.leases");
    let mut claimant = ctx(dir.path(), dead.clone());
    claimant.lease_file = Some(lease_file.clone());
    assert_eq!(claimant.claim(), EXIT_HELD);
    // The won lease is listed for the daemon.
    let listed = loom_daemon::pr_lease_reap::parse_file(&fs::read_to_string(&lease_file).unwrap());
    assert_eq!(listed, vec![claimant.held()]);
    // The run times out; the role runner reaps on the claimant's behalf.
    assert_eq!(ctx(dir.path(), dead.clone()).reap("timeout"), EXIT_HELD);
    let patch = patched(dir.path());
    assert!(
        patch.contains(&dead.released_marker_with(&format!(
            " head={HEAD} verdict=none labels=review-requested reaped=timeout"
        ))),
        "{patch}"
    );
    // Every other host's Judge is now refused before writing anything.
    for host in ["host-b", "host-c"] {
        assert_eq!(ctx(dir.path(), on(host, "run-2")).claim(), backstop::EXIT_BACKSTOP);
    }
    assert_eq!(posted(dir.path()), vec![dead.lease_marker()], "no peer wrote a lease");
    // A Doctor's claim is untouched by the Judge backstop.
    let doctor = Identity {
        role: Role::Doctor,
        ..on("host-b", "run-3")
    };
    assert_eq!(ctx(dir.path(), doctor).claim(), EXIT_HELD);
}

#[test]
fn release_drops_the_lease_from_the_runs_list_and_a_verdict_is_never_recorded() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    let lease_file = dir.path().join("run.leases");
    let mut c = ctx(dir.path(), on("host-a", "run-1"));
    c.lease_file = Some(lease_file.clone());
    assert_eq!(c.claim(), EXIT_HELD);
    assert_eq!(c.release(None), EXIT_HELD);
    assert_eq!(fs::read_to_string(&lease_file).unwrap(), "", "nothing left to reap");
    // A reap after a verdict landed frees the lease and records nothing.
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    fs::write(
        dir.path().join("pull"),
        format!(r#"{{"state":"open","head":"{HEAD}","labels":["loom:pr","loom:reviewing"]}}"#),
    )
    .unwrap();
    let a = on("host-a", "run-1");
    assert_eq!(ctx(dir.path(), a.clone()).claim(), EXIT_HELD);
    assert_eq!(ctx(dir.path(), a.clone()).reap("timeout"), EXIT_HELD);
    assert!(patched(dir.path()).contains(&format!("body={}", a.released_marker())));
    assert!(!patched(dir.path()).contains("verdict=none"));
}

/// Acceptance (#11931): a timed-out run counts toward the three Judge runs an
/// unchanged PR gets per 24 h.
#[test]
fn a_timed_out_judge_run_counts_toward_the_three_per_day_cap() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    waiting_pr(dir.path());
    // Two earlier passes at this head ended without a verdict, 23 h and 6 h
    // ago: the 4 h cool-off after the second is over, so a third Judge runs.
    let fields = format!(" head={HEAD} verdict=none labels=review-requested");
    let first = DateTime::parse_from_rfc3339(&stamp(Utc::now() - chrono::Duration::hours(23)))
        .unwrap()
        .with_timezone(&Utc);
    let second = Utc::now() - chrono::Duration::hours(6);
    let rows = row_at(10, first, &on("host-x", "s1").released_marker_with(&fields))
        + &row_at(20, second, &on("host-y", "s2").released_marker_with(&fields));
    fs::write(dir.path().join("rows"), rows).unwrap();
    let third = on("host-a", "run-3");
    assert_eq!(ctx(dir.path(), third.clone()).claim(), EXIT_HELD);
    // It times out and is reaped: the third record.
    assert_eq!(ctx(dir.path(), third).reap("timeout"), EXIT_HELD);
    // No host gets a fourth run until the first record leaves the window.
    let c = ctx(dir.path(), on("host-b", "run-4"));
    assert_eq!(c.claim(), backstop::EXIT_BACKSTOP);
    let rows = c.gh.read_rows(77, &c.policy).unwrap();
    assert_eq!(
        backstop::hold(&rows, c.gh.pr_head(77), Utc::now()),
        Some(first + chrono::Duration::hours(loom_daemon::judge_backstop::WINDOW_HOURS))
    );
}

/// Acceptance (#11931): two hosts reclaiming the same stale claim at once
/// produce exactly one winner. The dead claimant's lease has aged out; both
/// claimants pass the pre-check concurrently, and claim-then-verify-order
/// on comment id leaves one holder.
#[test]
fn two_hosts_reclaiming_one_stale_judge_claim_produce_one_winner() {
    for _ in 0..3 {
        let dir = TempDir::new().unwrap();
        fake_gh(dir.path());
        waiting_pr(dir.path());
        let dead = on("host-c", "run-dead");
        let aged = Utc::now() - chrono::Duration::minutes(20);
        fs::write(dir.path().join("rows"), row_at(50, aged, &dead.lease_marker())).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let racers: Vec<_> = [on("host-a", "run-a"), on("host-b", "run-b")]
            .into_iter()
            .map(|me| {
                let root = dir.path().to_path_buf();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    (me.clone(), ctx(&root, me).claim())
                })
            })
            .collect();
        let results: Vec<(Identity, i32)> = racers.into_iter().map(|t| t.join().unwrap()).collect();
        let winners: Vec<&Identity> = results
            .iter()
            .filter(|(_, c)| *c == EXIT_HELD)
            .map(|(w, _)| w)
            .collect();
        assert_eq!(winners.len(), 1, "{results:?}");
        assert!(
            results
                .iter()
                .all(|(_, c)| *c == EXIT_HELD || *c == EXIT_PEER),
            "{results:?}"
        );
        // The fences agree: only the winner may write a verdict.
        for (me, _) in &results {
            let want = if me == winners[0] {
                EXIT_HELD
            } else {
                EXIT_PEER
            };
            assert_eq!(ctx(dir.path(), me.clone()).fence(), want, "{me:?}");
        }
        // A third host arriving later stands down without writing.
        let before = posted(dir.path()).len();
        assert_eq!(ctx(dir.path(), on("host-d", "run-d")).claim(), EXIT_PEER);
        assert_eq!(posted(dir.path()).len(), before);
    }
}

// ---- prompt gates ---------------------------------------------------------

/// The role prompts gate on `lease pr claim` / `fence` exit codes. Only 0
/// (held) and 2 (an older `loom-daemon` without the verb: label-only) may
/// proceed; `[ $? -le 2 ]` also let exit 1 (`EXIT_STARTUP_FAILURE`, a new
/// binary that failed before dispatch) through, failing open (#9401 review).
#[test]
fn prompt_lease_gates_proceed_only_on_exit_0_or_2() {
    const GATE: &str = "[[ $? =~ ^[02]$ ]]";
    let prompts: [(&str, &str, usize); 4] = [
        (
            "doctor.md",
            include_str!("../../../../defaults/.claude/commands/loom/doctor.md"),
            6,
        ),
        (
            "judge.md",
            include_str!("../../../../defaults/.claude/commands/loom/judge.md"),
            8,
        ),
        (
            "loom-doctor/SKILL.md",
            include_str!("../../../../defaults/.agents/skills/loom-doctor/SKILL.md"),
            6,
        ),
        (
            "loom-judge/SKILL.md",
            include_str!("../../../../defaults/.agents/skills/loom-judge/SKILL.md"),
            8,
        ),
    ];
    for (name, text, gates) in prompts {
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            assert!(
                !(line.contains("$?") && line.contains("-le ")),
                "{name}:{}: a `-le` exit gate lets exit 1 through: {line}",
                i + 1
            );
            // Every claim/fence whose exit is tested (on its line or the
            // next) uses the 0-or-2 gate.
            if line.contains("loom-daemon lease pr claim")
                || line.contains("loom-daemon lease pr fence")
            {
                let tested = std::iter::once(*line)
                    .chain(lines.get(i + 1).copied())
                    .find(|l| l.contains("$?"));
                if let Some(t) = tested {
                    assert!(t.contains(GATE), "{name}:{}: gate must be `{GATE}`: {t}", i + 1);
                }
            }
        }
        assert_eq!(text.matches(GATE).count(), gates, "{name}: gate count");
    }
}

/// #11972: the Judge prompt's pending-CI skip releases its lease with the
/// pass's own reason, so the release never records it as a no-verdict pass.
#[test]
fn the_judge_prompt_releases_a_pending_ci_skip_with_its_reason() {
    let prompts = [
        ("judge.md", include_str!("../../../../defaults/.claude/commands/loom/judge.md")),
        (
            "loom-judge/SKILL.md",
            include_str!("../../../../defaults/.agents/skills/loom-judge/SKILL.md"),
        ),
    ];
    let flag = format!("--skip-reason {}", loom_daemon::judge_backstop::SKIP_CI_PENDING);
    for (name, text) in prompts {
        let lines: Vec<&str> = text.lines().collect();
        let case = lines
            .iter()
            .position(|l| l.trim_start().starts_with("LOOM-CHECKS-TIMEOUT*)"))
            .unwrap_or_else(|| panic!("{name}: no pending-CI skip case"));
        let arm: Vec<&str> = lines[case..]
            .iter()
            .copied()
            .take_while(|l| l.trim() != ";;")
            .collect();
        assert!(
            arm.iter()
                .any(|l| l.contains("loom-daemon lease pr release") && l.contains(&flag)),
            "{name}: the pending-CI skip must release with `{flag}`: {arm:?}"
        );
    }
}

// ---- step claims (#11898) -------------------------------------------------

#[test]
fn a_champion_lease_defaults_to_the_merge_step_and_carries_no_label() {
    let me = Identity::new(Role::Champion, "host-a".into(), "s".into(), None).unwrap();
    assert_eq!(me.step, Some(StepRole::ChampionMerge));
    assert_eq!(me.claim_label(), None);
    let marker = me.lease_marker();
    assert!(marker.ends_with("step=champion_merge -->"), "{marker}");
    assert_eq!(parse_record(&marker), Some(Record::Lease(me.clone())));
    // The yield record keeps the step inside the identity's fields.
    assert_eq!(parse_record(&me.yield_marker(&me)), Some(Record::Yield(me)));
}

#[test]
fn step_leases_name_their_claim_label() {
    let promote =
        Identity::new(Role::Champion, "h".into(), "s".into(), Some(StepRole::ChampionPromote))
            .unwrap();
    assert_eq!(promote.claim_label(), Some("loom:evaluating"));
    let curator = Identity::new(Role::Curator, "h".into(), "s".into(), None).unwrap();
    assert_eq!(curator.claim_label(), Some("loom:curating"));
    assert!(curator
        .lease_marker()
        .starts_with("<!-- loom:pr-lease role=curator host=h"));
    assert!(Identity::new(Role::Judge, "h".into(), "s".into(), Some(StepRole::Curator)).is_err());
}

/// An older binary's three-token marker still parses (no step), and a step
/// lease keys apart from a stepless one of the same identity.
#[test]
fn the_step_token_is_additive() {
    let old = who(Role::Judge, "s");
    assert_eq!(parse_record(&old.lease_marker()), Some(Record::Lease(old.clone())));
    let stepped =
        Identity::new(Role::Judge, old.host.clone(), "s".into(), Some(StepRole::Judge)).unwrap();
    // Ownership is (role, host, sweep): the step is metadata (#11950 review).
    assert_eq!(stepped, old);
    assert_ne!(stepped.step, old.step);
    let marker = stepped.lease_marker();
    let tokens: Vec<&str> = marker.split_whitespace().collect();
    assert_eq!(tokens[2..5], ["role=judge", "host=host-a1b2c3d4", "sweep=s"]);
}

#[test]
fn a_labelless_claim_renews_without_a_label_check() {
    let open = (true, Vec::<String>::new());
    let mut m = 0;
    assert_eq!(
        decide(true, fresh(10, 100), lease_renewer_exit_renew(), Some(&open), None, &mut m),
        Step::Renew
    );
    let closed = (false, Vec::<String>::new());
    assert!(matches!(
        decide(true, fresh(10, 100), lease_renewer_exit_renew(), Some(&closed), None, &mut m),
        Step::Stop(_)
    ));
}

/// The daemon's lib-side reader of this family (a dead step's lease holder,
/// #11898) agrees with this module's parser on every record shape.
#[test]
fn the_lib_role_lease_reader_agrees_with_parse_record() {
    use loom_daemon::sweep_registry::step_lease_owner::{parse_role_lease, RoleLeaseKind};
    let me = Identity::new(Role::Champion, "host-a".into(), "s1".into(), None).unwrap();
    let peer = who(Role::Judge, "s2");
    let bodies = [
        me.lease_marker(),
        me.yield_marker(&peer),
        me.released_marker(),
        peer.lease_marker(),
        peer.released_marker_with(" head=abc verdict=none"),
    ];
    for body in bodies {
        let (kind, holder) = match parse_record(&body).unwrap() {
            Record::Lease(h) => (RoleLeaseKind::Lease, h),
            Record::Yield(h) => (RoleLeaseKind::Yield, h),
            Record::Released(h) => (RoleLeaseKind::Released, h),
        };
        let lib = parse_role_lease(&body).unwrap();
        assert_eq!(lib.kind, kind, "{body}");
        assert_eq!(
            (lib.role.as_str(), lib.host.as_str(), lib.sweep.as_str()),
            (holder.role.as_str(), holder.host.as_str(), holder.sweep.as_str()),
            "{body}"
        );
    }
}

// ---- a daemon step's pre-taken lease (#11906) ---------------------------

#[path = "step_claim_tests.rs"]
mod step_claim;

#[path = "skip_reason_tests.rs"]
mod skip_reason;
/// #11950 review: a Champion that claimed with `--step champion_promote`
/// fences and releases its own lease with plain `--role champion` (default
/// step `champion_merge`) from the same host and sweep.
#[test]
fn a_step_less_fence_and_release_find_the_sessions_stepped_lease() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    let claimed = Identity::new(
        Role::Champion,
        "host-a".into(),
        "run-1".into(),
        Some(StepRole::ChampionPromote),
    )
    .unwrap();
    assert_eq!(ctx(dir.path(), claimed.clone()).claim(), EXIT_HELD);
    let plain = Identity::new(Role::Champion, "host-a".into(), "run-1".into(), None).unwrap();
    assert_eq!(plain.step, Some(StepRole::ChampionMerge), "the default step differs");
    assert_eq!(ctx(dir.path(), plain.clone()).fence(), EXIT_HELD, "own lease, not a peer's");
    assert_eq!(ctx(dir.path(), plain.clone()).check(), EXIT_HELD);
    assert_eq!(ctx(dir.path(), plain).release(None), EXIT_HELD);
    // The released marker keeps the lease's own step.
    assert!(
        patched(dir.path()).contains(&claimed.released_marker()),
        "{}",
        patched(dir.path())
    );
    // And the claim is free at once for a peer Champion.
    let peer = Identity::new(Role::Champion, "host-b".into(), "run-2".into(), None).unwrap();
    assert_eq!(ctx(dir.path(), peer).check(), EXIT_HELD);
}

/// #11950 review: a reap with no step (the role runner's) reaps a stepped lease.
#[test]
fn a_step_less_reap_reaps_a_stepped_lease() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    let stepped =
        Identity::new(Role::Judge, "host-a".into(), "run-1".into(), Some(StepRole::Judge)).unwrap();
    fs::write(dir.path().join("rows"), live_row(100, &stepped)).unwrap();
    let plain = Identity::new(Role::Judge, "host-a".into(), "run-1".into(), None).unwrap();
    assert_eq!(ctx(dir.path(), plain).reap("timeout"), EXIT_HELD);
    let patch = patched(dir.path());
    assert!(patch.contains("repos/acme/widgets/issues/comments/100"), "{patch}");
    assert!(
        patch.contains("loom:pr-lease-released role=judge host=host-a sweep=run-1 step=judge"),
        "{patch}"
    );
}

/// The issue-scoped leases never read a PR head on release (it would 404).
#[test]
fn an_issue_scoped_release_reads_no_pr_head() {
    let dir = TempDir::new().unwrap();
    fake_gh(dir.path());
    let curator = Identity::new(Role::Curator, "host-a".into(), "run-1".into(), None).unwrap();
    fs::write(dir.path().join("rows"), live_row(100, &curator)).unwrap();
    assert_eq!(ctx(dir.path(), curator.clone()).release(None), EXIT_HELD);
    assert!(patched(dir.path()).contains(&curator.released_marker()));
    assert!(!dir.path().join("pulled").exists(), "no PR read for a Curator lease");
}

/// #11950 review: the lib-side holder reader (a dead step's restore) and this
/// module agree that ownership ignores the step: a stepped lease yielded by a
/// step-less record of the same session holds nothing on either side, and the
/// next lease is the holder on both.
#[test]
fn the_lib_and_bin_lease_holders_agree_when_steps_differ() {
    use loom_daemon::sweep_registry::step_lease_owner::{earliest_live_holder, RoleLeaseRow};
    let stepped =
        Identity::new(Role::Judge, "host-a".into(), "run-1".into(), Some(StepRole::Judge)).unwrap();
    let plain = Identity::new(Role::Judge, "host-a".into(), "run-1".into(), None).unwrap();
    let next = Identity::new(Role::Judge, "host-b".into(), "run-2".into(), None).unwrap();
    let rows = vec![
        row(1, 1, stepped.lease_marker()),
        row(2, 1, next.lease_marker()),
        row(3, 1, plain.yield_marker(&next)),
    ];
    let bin = live_leases(&rows, Role::Judge, now(), ttl());
    assert_eq!(bin.first().map(|l| l.holder.sweep.as_str()), Some("run-2"));
    let lib_rows: Vec<RoleLeaseRow> = rows
        .iter()
        .map(|r| RoleLeaseRow {
            id: r.id,
            updated_at: r.updated_at,
            body: r.body.clone(),
        })
        .collect();
    assert_eq!(earliest_live_holder(&lib_rows, "judge", now(), ttl()).as_deref(), Some("run-2"));
}
