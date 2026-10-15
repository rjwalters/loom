use super::*;

use chrono::Duration;
use serde_json::json;

fn t0() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-15T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn names(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("host-{i}")).collect()
}

fn repos(n: usize) -> Vec<String> {
    (0..n).map(|i| format!("acme/repo-{i}")).collect()
}

/// A ring for `host` over `members`, sharding `repos`, confirmed at `t0`.
fn ring(host: &str, members: &[String], repos: &[String]) -> Ring {
    let mut members = members.to_vec();
    members.sort();
    Ring {
        commit: "c0ffee".to_string(),
        host: host.to_string(),
        members,
        repos: repos.iter().cloned().collect(),
        self_enabled: true,
        backup_every: DEFAULT_BACKUP_EVERY,
        max_stale_secs: DEFAULT_MAX_STALE_SECS,
        confirmed_at: t0(),
    }
}

fn verdict(host: &str, members: &[String], repos: &[String], slug: &str) -> Verdict {
    decide_with(Some(&ring(host, members, repos)), Some(slug), false, t0())
}

/// The owner of `slug` as every member sees it, asserting they agree.
fn owner_of(members: &[String], repos: &[String], slug: &str) -> String {
    let owners: Vec<&String> = members
        .iter()
        .filter(|h| verdict(h, members, repos, slug) == Verdict::Owner)
        .collect();
    assert_eq!(owners.len(), 1, "exactly one owner of {slug}: {owners:?}");
    owners[0].clone()
}

// ---------------------------------------------------------------------------
// Hash stability
// ---------------------------------------------------------------------------

#[test]
fn score_is_pinned() {
    // Pinned values: a change here re-homes every repo of every fleet at once
    // (all hosts must agree on these numbers, whatever their build). They
    // were computed outside Rust from the spec (FNV-1a 64 of
    // `member NUL repo`, then the murmur3 fmix64 finalizer).
    assert_eq!(score("host-0", "acme/repo-0"), 0xab3d_bfd8_a6d8_0e73);
    assert_eq!(score("host-1", "acme/repo-0"), 0xdb56_716d_be5f_ee22);
    assert_eq!(score("", ""), fmix64(crate::role_shard::hash_key("\u{0}")));
}

#[test]
fn score_separates_member_from_repo() {
    // The NUL separator: ("ab", "c") and ("a", "bc") are different inputs.
    assert_ne!(score("ab", "c"), score("a", "bc"));
}

#[test]
fn rank_ignores_member_order() {
    let members = names(6);
    let mut reversed = members.clone();
    reversed.reverse();
    for repo in repos(40) {
        assert_eq!(rank(&members, &repo), rank(&reversed, &repo));
    }
}

#[test]
fn every_repo_has_exactly_one_owner_and_one_backup() {
    let members = names(6);
    let repos = repos(63);
    for slug in &repos {
        let verdicts: Vec<Verdict> = members
            .iter()
            .map(|h| verdict(h, &members, &repos, slug))
            .collect();
        let owners = verdicts.iter().filter(|v| **v == Verdict::Owner).count();
        let backups = verdicts
            .iter()
            .filter(|v| matches!(v, Verdict::Backup { .. }))
            .count();
        assert_eq!((owners, backups), (1, 1), "{slug}: {verdicts:?}");
    }
}

#[test]
fn ownership_spreads_over_the_members() {
    let members = names(6);
    let repos = repos(600);
    let mut counts: HashMap<String, usize> = HashMap::new();
    for slug in &repos {
        *counts
            .entry(rank(&members, slug)[0].to_string())
            .or_default() += 1;
    }
    // 100 each on average; a skewed hash would starve or flood a host.
    for m in &members {
        let n = counts.get(m).copied().unwrap_or_default();
        assert!((60..=140).contains(&n), "{m} owns {n} of 600: {counts:?}");
    }
}

#[test]
fn slug_case_does_not_change_the_owner() {
    let members = names(3);
    let repos = vec!["acme/widget".to_string()];
    let a = decide_with(Some(&ring("host-0", &members, &repos)), Some("Acme/Widget"), false, t0());
    let b = decide_with(Some(&ring("host-0", &members, &repos)), Some("acme/widget"), false, t0());
    assert_eq!(a, b);
}

// ---------------------------------------------------------------------------
// Host-set change
// ---------------------------------------------------------------------------

#[test]
fn a_leaving_host_moves_only_its_repos_to_their_backups() {
    let before = names(6);
    let repos = repos(120);
    let leaving = "host-2".to_string();
    let after: Vec<String> = before.iter().filter(|h| **h != leaving).cloned().collect();
    for slug in &repos {
        let was = owner_of(&before, &repos, slug);
        let now = owner_of(&after, &repos, slug);
        if was == leaving {
            assert_eq!(now, rank(&before, slug)[1], "{slug} goes to its backup");
        } else {
            assert_eq!(now, was, "{slug} did not belong to the leaver and must not move");
        }
    }
}

#[test]
fn a_joining_host_takes_repos_only_from_others_onto_itself() {
    let before = names(5);
    let mut after = before.clone();
    after.push("host-new".to_string());
    let repos = repos(120);
    let mut taken = 0;
    for slug in &repos {
        let was = owner_of(&before, &repos, slug);
        let now = owner_of(&after, &repos, slug);
        if now != was {
            assert_eq!(now, "host-new", "{slug} moved somewhere other than the joiner");
            taken += 1;
        }
    }
    // About 1/6 of 120; never none, never most.
    assert!((8..=40).contains(&taken), "joiner took {taken} of 120");
}

// ---------------------------------------------------------------------------
// Fallback when the owner is gone
// ---------------------------------------------------------------------------

const STORE: &str = r#"{
  "_generated": {"schema_version": 1},
  "root": "~/src",
  "repos": [
    {"name": "a", "remote": "git@github.com:Acme/A.git", "fleet": true},
    {"name": "b", "remote": "https://github.com/acme/b", "fleet": "maintain"},
    {"name": "c", "remote": "git@github.com:acme/c.git", "fleet": false},
    {"name": "d", "remote": "git@github.com:acme/d.git", "fleet": true, "firewall": true}
  ],
  "state": {
    "fleet": {"state": "running"},
    "hosts": {"host-p": {"state": "paused"}, "host-s": {"state": "stopped"}}
  },
  "config": {
    "defaults": {"fleet": {"autoApply": true}},
    "hosts": {
      "host-a": {"defaults": {"autonomous": {"passShard": {"enabled": true, "backupEvery": 3}}}},
      "host-b": {"defaults": {"autonomous": {"passShard": {"enabled": true}}}},
      "host-c": {"defaults": {"autonomous": {"passShard": {"enabled": true}}}},
      "host-p": {"defaults": {"autonomous": {"passShard": {"enabled": true}}}},
      "host-s": {"defaults": {"autonomous": {"passShard": {"enabled": true}}}},
      "host-off": {"defaults": {}},
      "host-noapply": {"defaults": {"fleet": {"autoApply": false},
                                    "autonomous": {"passShard": {"enabled": true}}}},
      "host-local-only": {"local": {"autonomous": {"passShard": {"enabled": true}}}}
    }
  }
}"#;

fn store_ring(text: &str, host: &str) -> Ring {
    let doc = crate::fleet_store::compiled::parse(text).unwrap();
    let state = |h: &str| {
        crate::fleet_store::state::resolve_value(doc.state(), h, "test")
            .ok()
            .map(|s| s.state)
    };
    ring_from_compiled(&doc, host, "c0ffee", t0(), &state)
}

#[test]
fn members_are_enabled_auto_applying_running_hosts() {
    let ring = store_ring(STORE, "host-a");
    assert_eq!(ring.members, vec!["host-a", "host-b", "host-c"]);
    assert!(ring.self_enabled);
    assert_eq!(ring.backup_every, 3);
    assert_eq!(ring.max_stale_secs, DEFAULT_MAX_STALE_SECS);
}

#[test]
fn only_dispatched_or_maintained_non_firewall_repos_are_sharded() {
    let ring = store_ring(STORE, "host-a");
    let want: BTreeSet<String> = ["acme/a", "acme/b"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    assert_eq!(ring.repos, want);
    let local = decide_with(Some(&ring), Some("acme/c"), false, t0());
    assert_eq!(local, Verdict::Unsharded(Unsharded::NotInRoster));
}

#[test]
fn a_paused_owner_hands_its_repos_to_the_backup() {
    let running = store_ring(STORE, "host-a");
    let slug = "acme/a";
    let ranked = rank(&running.members, slug);
    let (owner, backup) = (ranked[0].to_string(), ranked[1].to_string());

    let mut doc: Value = serde_json::from_str(STORE).unwrap();
    doc["state"]["hosts"][&owner] = json!({"state": "paused"});
    let paused = doc.to_string();

    let after = store_ring(&paused, &backup);
    assert!(!after.members.contains(&owner), "{owner} left the ring");
    assert_eq!(decide_with(Some(&after), Some(slug), false, t0()), Verdict::Owner);
    // The paused host itself is no longer a member, so it runs everything.
    let own = store_ring(&paused, &owner);
    assert_eq!(
        decide_with(Some(&own), Some(slug), false, t0()),
        Verdict::Unsharded(Unsharded::NotMember)
    );
}

#[test]
fn an_owner_removed_from_the_ring_is_replaced_by_the_backup() {
    let members = names(4);
    let repos = repos(30);
    for slug in &repos {
        let ranked = rank(&members, slug);
        let (owner, backup) = (ranked[0].to_string(), ranked[1].to_string());
        assert!(matches!(
            verdict(&backup, &members, &repos, slug),
            Verdict::Backup { owner: ref o, .. } if *o == owner
        ));
        let survivors: Vec<String> = members.iter().filter(|h| **h != owner).cloned().collect();
        assert_eq!(verdict(&backup, &survivors, &repos, slug), Verdict::Owner);
    }
}

#[test]
fn a_backup_runs_every_nth_due_slot() {
    let key = "test/backup-slot-cadence";
    let ran: Vec<bool> = (0..7).map(|_| backup_slot_due(key, 3)).collect();
    assert_eq!(ran, vec![true, false, false, true, false, false, true]);
    // `every = 0` is treated as 1: never a backup that never runs.
    let key = "test/backup-slot-zero";
    assert!((0..3).all(|_| backup_slot_due(key, 0)));
}

// ---------------------------------------------------------------------------
// Unsharded reasons
// ---------------------------------------------------------------------------

#[test]
fn every_doubt_runs_the_pass() {
    let members = names(3);
    let repos = repos(3);
    let slug = Some(repos[0].as_str());
    let base = ring("host-0", &members, &repos);

    assert_eq!(decide_with(None, slug, false, t0()), Verdict::Unsharded(Unsharded::NoRing));
    assert_eq!(
        decide_with(Some(&base), slug, true, t0()),
        Verdict::Unsharded(Unsharded::KillSwitch)
    );
    let off = Ring {
        self_enabled: false,
        ..base.clone()
    };
    assert_eq!(decide_with(Some(&off), slug, false, t0()), Verdict::Unsharded(Unsharded::Off));
    let outsider = Ring {
        host: "elsewhere".to_string(),
        ..base.clone()
    };
    assert_eq!(
        decide_with(Some(&outsider), slug, false, t0()),
        Verdict::Unsharded(Unsharded::NotMember)
    );
    let alone = ring("host-0", &names(1), &repos);
    assert_eq!(
        decide_with(Some(&alone), slug, false, t0()),
        Verdict::Unsharded(Unsharded::TooFewMembers)
    );
    assert_eq!(
        decide_with(Some(&base), None, false, t0()),
        Verdict::Unsharded(Unsharded::NotInRoster)
    );
    let late = t0() + Duration::seconds(DEFAULT_MAX_STALE_SECS + 1);
    assert_eq!(
        decide_with(Some(&base), slug, false, late),
        Verdict::Unsharded(Unsharded::Stale)
    );
}

#[test]
fn a_stale_but_recent_snapshot_stays_sharded() {
    // An unreachable forge leaves every host on the same snapshot; unsharding
    // then would multiply the load while GitHub is rate-limiting.
    let members = names(3);
    let repos = repos(3);
    let base = ring("host-0", &members, &repos);
    let hour_later = t0() + Duration::hours(1);
    assert!(!matches!(
        decide_with(Some(&base), Some(&repos[0]), false, hour_later),
        Verdict::Unsharded(_)
    ));
}

#[test]
fn a_host_with_only_a_local_tier_is_not_a_member() {
    let ring = store_ring(STORE, "host-local-only");
    assert!(!ring.self_enabled);
    assert!(!ring.members.contains(&"host-local-only".to_string()));
}

#[test]
fn a_store_without_compiled_hosts_has_no_members() {
    let text = json!({
        "_generated": {"schema_version": 1},
        "root": "~/src",
        "repos": [],
        "state": {"fleet": {"state": "running"}},
        "config": {"defaults": {}}
    })
    .to_string();
    let ring = store_ring(&text, "host-a");
    assert!(ring.members.is_empty());
    assert_eq!(
        decide_with(Some(&ring), Some("acme/a"), false, t0()),
        Verdict::Unsharded(Unsharded::Off)
    );
}
