//! Tests for `loom-daemon lease pr` (#9401).
//!
//! The ordering, yield and fence decisions are pure and exercised directly;
//! `claim` is then driven end to end against a FAKE `gh` that keeps a
//! comment list on disk, so the pre-check, the post, the read-back and the
//! yield record are observed exactly as a claimant sees them.

use super::renew::{decide, Step, LABEL_MISS_LIMIT};
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
    assert_eq!(decide(true, 10, 100, ok, Some(&open), "loom:treating", &mut m), Step::Renew);
    assert!(matches!(
        decide(false, 10, 100, ok, Some(&open), "loom:treating", &mut m),
        Step::Stop(_)
    ));
    assert!(matches!(
        decide(true, 100, 100, ok, Some(&open), "loom:treating", &mut m),
        Step::Stop(_)
    ));
    assert!(matches!(
        decide(true, 10, 100, 3, Some(&open), "loom:treating", &mut m),
        Step::Stop(_)
    ));
    assert!(matches!(
        decide(true, 10, 100, ok, Some(&closed), "loom:treating", &mut m),
        Step::Stop(_)
    ));
    assert_eq!(decide(true, 10, 100, ok, None, "loom:treating", &mut m), Step::Skip);
    // The label must be gone for LABEL_MISS_LIMIT consecutive cycles.
    m = 0;
    for _ in 1..LABEL_MISS_LIMIT {
        assert_eq!(decide(true, 10, 100, ok, Some(&bare), "loom:treating", &mut m), Step::Skip);
    }
    assert!(matches!(
        decide(true, 10, 100, ok, Some(&bare), "loom:treating", &mut m),
        Step::Stop(_)
    ));
    m = 1;
    assert_eq!(decide(true, 10, 100, ok, Some(&open), "loom:treating", &mut m), Step::Renew);
    assert_eq!(m, 0, "a present label resets the streak");
}

fn lease_renewer_exit_renew() -> i32 {
    super::super::lease_renewer::EXIT_RENEW
}

// ---- claim end to end, against a fake gh --------------------------------

const AUTHOR: &str =
    r#""user":{"login":"loom-fleet-dispatch[bot]","type":"Bot"},"author_association":"NONE""#;

/// A fake `gh` keeping the PR's comments in `<dir>/rows` (NDJSON). A POST
/// appends a row holding the body's marker line; rows in `<dir>/late` join
/// the list after the first comments read (a peer racing the pre-check).
fn fake_gh(dir: &Path) -> PathBuf {
    let d = dir.display();
    let script = format!(
        r#"#!/usr/bin/env bash
D="{d}"
[ -f "$D/fail" ] && {{ echo 'HTTP 502' >&2; exit 1; }}
case " $* " in
  *" POST "*)
    prev=""; body=""
    for a in "$@"; do [ "$prev" = "-f" ] && body="${{a#body=}}"; prev="$a"; done
    first="${{body%%$'\n'*}}"
    id=$(cat "$D/next"); echo $((id + 1)) > "$D/next"
    printf '{{"id":%s,"updated_at":"%s","body":"%s",%s}}\n' "$id" "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$first" '{AUTHOR}' >> "$D/rows"
    echo "$first" >> "$D/posted"
    echo "$id" ;;
  *" PATCH "*) echo "$*" >> "$D/patched" ;;
  *"/pulls/"*) echo "" ;;
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
    assert_eq!(ctx(dir.path(), a.clone()).release(), EXIT_HELD);
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
