use super::super::workspace_volume_name;
use super::*;
use std::cell::Cell;
use std::rc::Rc;

const ALICE_ID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BOB_ID: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const UNVERIFIED_ID: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

fn container_state(id: &str, account: &str, running: bool, mount: &str) -> String {
    format!("{id}\t{running}\tfalse\tprivate-clone\t{account}\n\"/workspace\"\tvolume\t{mount}")
}

#[test]
fn seat_volume_usage_uses_verified_mounts_and_totals_complete_inventory() {
    let _fake = docker::fake::install(Box::new(|args| {
        let reply = match args {
            ["volume", "ls", "--filter", "name=loom-codex-workspace-", "--format", "{{.Name}}"] => {
                "unmanaged-work\nloom-codex-workspace-bob\nloom-codex-workspace-alice\n".into()
            }
            ["volume", "inspect", "--format", VOLUME_FORMAT, volume] => {
                let account = volume.strip_prefix(VOLUME_PREFIX).unwrap();
                format!("{volume}\tlocal\t{account}\t0\n")
            }
            ["container", "inspect", "--format", CONTAINER_FORMAT, "loom-codex-session-alice"] => {
                container_state(ALICE_ID, "alice", true, "loom-codex-workspace-alice")
            }
            ["container", "inspect", "--format", CONTAINER_FORMAT, "loom-codex-session-bob"] => {
                container_state(BOB_ID, "bob", true, "loom-codex-workspace-bob")
            }
            ["exec", "--user", "1000:1000", ALICE_ID, "/usr/bin/timeout", "--kill-after=1s", "50s", "/usr/bin/du", "-skx", "--", ROOT] => {
                "12\t/workspace\n".into()
            }
            ["exec", "--user", "1000:1000", BOB_ID, "/usr/bin/timeout", "--kill-after=1s", "50s", "/usr/bin/du", "-skx", "--", ROOT] => {
                "0\t/workspace\n".into()
            }
            unexpected => panic!("unexpected Docker request: {unexpected:?}"),
        };
        Ok(reply)
    }));
    let mut samples = SeatVolumeSamples::default();
    assert_eq!(
        samples.sample(),
        SeatVolumeUsage {
            seats: vec![("alice".into(), 12_288)],
            total: None
        }
    );
    assert_eq!(
        samples.sample(),
        SeatVolumeUsage {
            seats: vec![("bob".into(), 0)],
            total: Some(12_288)
        }
    );
    assert!(docker::fake::seen_env().iter().all(Vec::is_empty));
}

#[test]
fn seat_volume_usage_keeps_successes_without_a_partial_total() {
    let _fake = docker::fake::install(Box::new(|args| match args {
        ["volume", "ls", ..] => Ok([
            "alice",
            "bob",
            "missing",
            "wrong",
            "nested",
            "external",
            "unreadable",
        ]
        .iter()
        .map(|account| format!("{VOLUME_PREFIX}{account}"))
        .collect::<Vec<_>>()
        .join("\n")),
        ["volume", "inspect", "--format", VOLUME_FORMAT, "loom-codex-workspace-missing"] => {
            anyhow::bail!("volume disappeared")
        }
        ["volume", "inspect", "--format", VOLUME_FORMAT, volume] => {
            let account = volume.strip_prefix(VOLUME_PREFIX).unwrap();
            let options = u8::from(account == "external");
            Ok(format!("{volume}\tlocal\t{account}\t{options}"))
        }
        ["container", "inspect", "--format", CONTAINER_FORMAT, "loom-codex-session-alice"] => {
            Ok(container_state(ALICE_ID, "alice", true, "loom-codex-workspace-alice"))
        }
        ["container", "inspect", "--format", CONTAINER_FORMAT, "loom-codex-session-bob"] => {
            Ok(container_state(UNVERIFIED_ID, "bob", false, "loom-codex-workspace-bob"))
        }
        ["container", "inspect", "--format", CONTAINER_FORMAT, "loom-codex-session-wrong"] => {
            Ok(container_state(UNVERIFIED_ID, "wrong", true, "loom-codex-workspace-bob"))
        }
        ["container", "inspect", "--format", CONTAINER_FORMAT, "loom-codex-session-nested"] => {
            Ok(container_state(UNVERIFIED_ID, "nested", true, "loom-codex-workspace-nested")
                + "\n\"/workspace/extra directory\"\tbind\t")
        }
        ["container", "inspect", "--format", CONTAINER_FORMAT, "loom-codex-session-unreadable"] => {
            Ok(container_state(BOB_ID, "unreadable", true, "loom-codex-workspace-unreadable"))
        }
        ["exec", "--user", "1000:1000", ALICE_ID, ..] => Ok("3\t/workspace\n".into()),
        ["exec", "--user", "1000:1000", BOB_ID, ..] => anyhow::bail!("read failed"),
        unexpected => panic!("unverified target reached Docker: {unexpected:?}"),
    }));
    let mut samples = SeatVolumeSamples::default();
    assert_eq!(
        samples.sample(),
        SeatVolumeUsage {
            seats: vec![("alice".into(), 3072)],
            total: None
        }
    );
    for _ in 0..6 {
        assert_eq!(samples.sample(), SeatVolumeUsage::default());
    }
    // Only the two verified mounts reached exec, even though one read failed.
    assert_eq!(docker::fake::seen_env().len(), 21);
}

#[test]
fn seat_volume_usage_does_not_measure_after_the_budget_or_fabricate_an_inventory() {
    let mut samples = SeatVolumeSamples::default();
    {
        let _fake = docker::fake::install(Box::new(|_| panic!("budget exhausted")));
        assert_eq!(samples.sample_until(Instant::now(), Instant::now), SeatVolumeUsage::default());
    }
    let _fake = docker::fake::install(Box::new(|args| {
        assert!(matches!(args, ["volume", "ls", ..]));
        anyhow::bail!("inventory unavailable")
    }));
    assert_eq!(samples.sample(), SeatVolumeUsage::default());
}

#[test]
fn seat_volume_usage_parses_only_one_bounded_root_measurement() {
    for output in [
        "12\t/home/loom",
        "1\t/workspace\n2\t/workspace",
        "-1\t/workspace",
        "18446744073709551615\t/workspace",
    ] {
        assert_eq!(parse_du(output), None, "{output}");
    }
}

#[test]
fn seat_volume_usage_rotates_seven_measured_cold_reads_into_a_total() {
    const COLD_WALK: Duration = Duration::from_millis(32_815);
    const REPORTED_KIB: i64 = 39_786_660;
    let start = Instant::now();
    let clock = Rc::new(Cell::new(start));
    let elapsed = Rc::clone(&clock);
    let _fake = docker::fake::install(Box::new(move |args| {
        elapsed.set(elapsed.get() + Duration::from_millis(18));
        match args {
            ["volume", "ls", ..] => Ok((1..=7)
                .map(|n| workspace_volume_name(&format!("seat-{n}")))
                .collect::<Vec<_>>()
                .join("\n")),
            ["volume", "inspect", "--format", VOLUME_FORMAT, volume] => {
                let account = volume.strip_prefix(VOLUME_PREFIX).unwrap();
                Ok(format!("{volume}\tlocal\t{account}\t0"))
            }
            ["container", "inspect", "--format", CONTAINER_FORMAT, container] => {
                let account = container.strip_prefix("loom-codex-session-").unwrap();
                Ok(container_state(ALICE_ID, account, true, &workspace_volume_name(account)))
            }
            ["exec", "--user", "1000:1000", ALICE_ID, "/usr/bin/timeout", "--kill-after=1s", cap, "/usr/bin/du", "-skx", "--", ROOT] =>
            {
                let cap = Duration::from_secs(cap.strip_suffix('s').unwrap().parse().unwrap());
                if cap < COLD_WALK {
                    anyhow::bail!("modeled cold walk hit the in-container timeout");
                }
                elapsed.set(elapsed.get() + COLD_WALK);
                Ok(format!("{REPORTED_KIB}\t/workspace\n"))
            }
            unexpected => panic!("unexpected Docker request: {unexpected:?}"),
        }
    }));
    let mut samples = SeatVolumeSamples::default();
    for i in 0..7 {
        let tick = start + Duration::from_secs(i * 60);
        clock.set(tick);
        let result = samples.sample_until(tick + SAMPLE_BUDGET, || clock.get());
        assert!(clock.get() < tick + SAMPLE_BUDGET);
        assert_eq!(result.seats, [(format!("seat-{}", i + 1), REPORTED_KIB * 1024)]);
        assert_eq!(result.total, (i == 6).then_some(7 * REPORTED_KIB * 1024));
    }
    assert_eq!(docker::fake::seen_env().len(), 7 * 4);
}

#[test]
fn seat_volume_usage_invalidates_failed_expired_and_removed_readings() {
    #[derive(Clone, Copy)]
    enum Inventory {
        All,
        Alice,
        Bob,
        Missing,
    }
    let inventory = Rc::new(Cell::new(Inventory::All));
    let fail = Rc::new(Cell::new(false));
    let roster = Rc::clone(&inventory);
    let failed = Rc::clone(&fail);
    let _fake = docker::fake::install(Box::new(move |args| match args {
        ["volume", "ls", ..] => match roster.get() {
            Inventory::All => Ok("loom-codex-workspace-alice\nloom-codex-workspace-bob".into()),
            Inventory::Alice => Ok("loom-codex-workspace-alice".into()),
            Inventory::Bob => Ok("loom-codex-workspace-bob".into()),
            Inventory::Missing => anyhow::bail!("cannot list volumes"),
        },
        ["volume", "inspect", "--format", VOLUME_FORMAT, volume] => {
            let account = volume.strip_prefix(VOLUME_PREFIX).unwrap();
            Ok(format!("{volume}\tlocal\t{account}\t0"))
        }
        ["container", "inspect", "--format", CONTAINER_FORMAT, "loom-codex-session-alice"] => {
            Ok(container_state(ALICE_ID, "alice", true, "loom-codex-workspace-alice"))
        }
        ["container", "inspect", "--format", CONTAINER_FORMAT, "loom-codex-session-bob"] => {
            Ok(container_state(BOB_ID, "bob", true, "loom-codex-workspace-bob"))
        }
        ["exec", "--user", "1000:1000", ALICE_ID, ..] if failed.get() => {
            anyhow::bail!("du saw a file disappear, despite producing a total");
        }
        ["exec", "--user", "1000:1000", ALICE_ID | BOB_ID, ..] => Ok("3\t/workspace\n".into()),
        unexpected => panic!("unexpected Docker request: {unexpected:?}"),
    }));
    let start = Instant::now();
    let mut samples = SeatVolumeSamples::default();
    let mut at = |elapsed| {
        let now = start + Duration::from_secs(elapsed);
        samples.sample_until(now + SAMPLE_BUDGET, || now)
    };
    assert_eq!(at(0).total, None);
    assert_eq!(at(60).total, Some(6144));
    fail.set(true);
    assert_eq!(at(120), SeatVolumeUsage::default());
    assert_eq!(at(180).total, None);
    fail.set(false);
    assert_eq!(at(240).total, Some(6144));
    assert_eq!(at(1140).total, None); // Alice is exactly 15 minutes old.
    assert_eq!(at(1200).total, Some(6144));

    inventory.set(Inventory::Bob);
    assert_eq!(at(1260).total, Some(3072));
    inventory.set(Inventory::All);
    assert_eq!(at(1320).total, Some(6144));

    inventory.set(Inventory::Alice);
    assert_eq!(at(1380).total, Some(3072));
    inventory.set(Inventory::All);
    assert_eq!(at(1440).total, Some(6144));
    inventory.set(Inventory::Missing);
    assert_eq!(at(1500), SeatVolumeUsage::default());
    inventory.set(Inventory::All);
    assert_eq!(at(1560).total, None);
}
