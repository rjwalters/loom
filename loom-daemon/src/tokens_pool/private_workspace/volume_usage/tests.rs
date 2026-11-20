use super::super::workspace_volume_name;
use super::*;
use std::cell::Cell;
use std::rc::Rc;

const IMAGE: &str = "local/session-test:fixture";
const ENGINE_ROOT: &str = "/var/lib/docker";
const CREATED: &str = "2026-11-19T23:30:00Z";

fn trusted() -> Option<String> {
    Some(IMAGE.into())
}

fn infos(volumes: &[&str], created: &str) -> String {
    volumes
        .iter()
        .map(|volume| volume_info(volume, volume.strip_prefix(VOLUME_PREFIX).unwrap(), 0, created))
        .collect()
}

fn volume_info(volume: &str, account: &str, options: u8, created: &str) -> String {
    format!(
        "{volume}\tlocal\t{account}\t{options}\t{ENGINE_ROOT}/volumes/{volume}/_data\t{created}\n"
    )
}

#[test]
fn seat_volume_usage_measures_volumes_without_a_session_and_totals_complete_inventory() {
    let _fake = docker::fake::install(Box::new(|args| {
        let reply = match args {
            ["volume", "ls", "--filter", "name=loom-codex-workspace-", "--format", "{{.Name}}"] => {
                "unmanaged-work\nloom-codex-workspace-idle\nloom-codex-workspace-active\n".into()
            }
            ["volume", "inspect", "--format", VOLUME_FORMAT, volumes @ ..] => {
                infos(volumes, CREATED)
            }
            ["info", "--format", "{{.DockerRootDir}}"] => format!("{ENGINE_ROOT}\n"),
            ["run", "--rm", "--pull", "never", "--network", "none", "--user", "1000:1000", "--read-only", "--cap-drop", "ALL", "--security-opt", "no-new-privileges", "--log-driver", "none", "--workdir", "/", "--mount", mount, "--entrypoint", "/usr/bin/timeout", "--", IMAGE, "--kill-after=1s", cap, "/usr/bin/du", "-skx", "--", ROOT] =>
            {
                assert!(
                    (1..=MAX_DU_SECS).contains(&cap.strip_suffix('s').unwrap().parse().unwrap())
                );
                match *mount {
                    "type=bind,src=/var/lib/docker/volumes/loom-codex-workspace-active/_data,dst=/workspace,readonly,bind-recursive=disabled" =>
                        "12\t/workspace\n".into(),
                    "type=bind,src=/var/lib/docker/volumes/loom-codex-workspace-idle/_data,dst=/workspace,readonly,bind-recursive=disabled" =>
                        "0\t/workspace\n".into(),
                    _ => panic!("unexpected mount: {mount}"),
                }
            }
            unexpected => panic!("unexpected Docker request: {unexpected:?}"),
        };
        Ok(reply)
    }));
    let mut samples = SeatVolumeSamples::default();
    assert_eq!(
        samples.sample(trusted),
        SeatVolumeUsage {
            seats: vec![("active".into(), 12_288)],
            total: None
        }
    );
    assert_eq!(
        samples.sample(trusted),
        SeatVolumeUsage {
            seats: vec![("idle".into(), 0)],
            total: Some(12_288)
        }
    );
    assert!(docker::fake::seen_env().iter().all(Vec::is_empty));
}

#[test]
fn seat_volume_usage_rejects_nonlocal_nonowned_and_indirect_backing_paths() {
    let volume = "loom-codex-workspace-alice";
    let exact = volume_info(volume, "alice", 0, CREATED);
    assert_eq!(
        verified_volume(&exact, volume, "alice", ENGINE_ROOT),
        Some("/var/lib/docker/volumes/loom-codex-workspace-alice/_data")
    );
    for invalid in [
        volume_info(volume, "bob", 0, CREATED),
        volume_info(volume, "alice", 1, CREATED),
        volume_info(volume, "alice", 0, ""),
        exact.replace("\tlocal\t", "\tnfs\t"),
        exact.replace("/_data", "/../other/_data"),
        exact.replace("/var/lib/docker", "/etc"),
        exact.replace("/_data", "/_data,rw"),
        format!("other\n{exact}"),
    ] {
        assert_eq!(verified_volume(&invalid, volume, "alice", ENGINE_ROOT), None, "{invalid:?}");
    }
    for root in [
        "",
        "/",
        ".",
        "var/lib/docker",
        "/var/../etc",
        "/var/lib/docker,rw",
    ] {
        assert_eq!(verified_volume(&exact, volume, "alice", root), None, "{root:?}");
    }
}

#[test]
fn seat_volume_usage_drops_a_reading_if_the_volume_is_replaced_during_the_read() {
    let replaced = Rc::new(Cell::new(false));
    let flag = Rc::clone(&replaced);
    let _fake = docker::fake::install(Box::new(move |args| match args {
        ["volume", "ls", ..] => Ok("loom-codex-workspace-alice\n".into()),
        ["info", "--format", "{{.DockerRootDir}}"] => Ok(ENGINE_ROOT.into()),
        ["volume", "inspect", "--format", VOLUME_FORMAT, volumes @ ..] => Ok(infos(
            volumes,
            if flag.get() {
                "2026-11-19T23:35:00Z"
            } else {
                CREATED
            },
        )),
        ["run", ..] => {
            flag.set(true);
            Ok("3\t/workspace\n".into())
        }
        unexpected => panic!("unexpected Docker request: {unexpected:?}"),
    }));
    let mut samples = SeatVolumeSamples::default();
    assert_eq!(samples.sample(trusted), SeatVolumeUsage::default());
    assert!(samples.readings.is_empty());
}

#[test]
fn seat_volume_usage_withholds_the_total_when_a_volume_is_replaced_between_turns() {
    let alice_created = Rc::new(std::cell::RefCell::new(CREATED.to_owned()));
    let created = Rc::clone(&alice_created);
    let _fake = docker::fake::install(Box::new(move |args| match args {
        ["volume", "ls", ..] => Ok("loom-codex-workspace-alice\nloom-codex-workspace-bob\n".into()),
        ["info", "--format", "{{.DockerRootDir}}"] => Ok(ENGINE_ROOT.into()),
        ["volume", "inspect", "--format", VOLUME_FORMAT, volumes @ ..] => Ok(volumes
            .iter()
            .map(|volume| {
                let account = volume.strip_prefix(VOLUME_PREFIX).unwrap();
                let at = if account == "alice" {
                    created.borrow().clone()
                } else {
                    CREATED.into()
                };
                volume_info(volume, account, 0, &at)
            })
            .collect()),
        ["run", .., mount, "--entrypoint", _, "--", _, _, _, _, _, _, _] => {
            let alice = mount.contains("-alice/");
            let replaced = created.borrow().as_str() != CREATED;
            Ok(match (alice, replaced) {
                (true, false) => "100\t/workspace\n",
                (true, true) => "1\t/workspace\n",
                (false, _) => "2\t/workspace\n",
            }
            .into())
        }
        unexpected => panic!("unexpected Docker request: {unexpected:?}"),
    }));
    let mut samples = SeatVolumeSamples::default();
    assert_eq!(samples.sample(trusted).seats, [("alice".to_owned(), 102_400)]);
    *alice_created.borrow_mut() = "2026-11-19T23:35:00Z".into();
    // Bob is measured while Alice's retained 100 KiB describes a replaced volume.
    let bob = samples.sample(trusted);
    assert_eq!(
        bob,
        SeatVolumeUsage {
            seats: vec![("bob".into(), 2048)],
            total: None
        }
    );
    let alice = samples.sample(trusted);
    assert_eq!(alice.seats, [("alice".to_owned(), 1024)]);
    assert_eq!(alice.total, Some(3072));
}

#[test]
fn reader_image_is_refused_before_any_workspace_is_mounted() {
    let _fake = docker::fake::install(Box::new(|args| match args {
        ["image", "inspect", ..] => Ok("sha256:0\n".into()),
        // The bundle probe: an image without the sealed control bundle.
        ["run", .., "--entrypoint", "/bin/sh", _, "-c", _] => {
            Ok("__LOOM_BUNDLE_NO_MANIFEST__\n".into())
        }
        unexpected => panic!("unexpected Docker request: {unexpected:?}"),
    }));
    assert_eq!(reader_image(Some(IMAGE)), None);
    let _absent = docker::fake::install(Box::new(|args| match args {
        ["image", "inspect", ..] => anyhow::bail!("no such image"),
        unexpected => panic!("pulled or probed an absent image: {unexpected:?}"),
    }));
    assert_eq!(reader_image(Some(IMAGE)), None);
}

#[test]
fn stock_reader_candidates_include_versioned_tags_newest_first() {
    let _fake = docker::fake::install(Box::new(|args| {
        match args {
        ["image", "ls", "--format", "{{.Repository}}:{{.Tag}}", "ghcr.io/2amlogic/loom-worker-session"] => Ok(
            "ghcr.io/2amlogic/loom-worker-session:0.19.1100\nghcr.io/2amlogic/loom-worker-session:<none>\nghcr.io/2amlogic/loom-worker-session:0.19.1209\nghcr.io/2amlogic/loom-worker-session:latest\nghcr.io/2amlogic/loom-worker-session:foo\n".into(),
        ),
        unexpected => panic!("unexpected Docker request: {unexpected:?}"),
    }
    }));
    assert_eq!(
        local_stock_images(),
        [
            "ghcr.io/2amlogic/loom-worker-session:latest",
            "ghcr.io/2amlogic/loom-worker-session:0.19.1209",
            "ghcr.io/2amlogic/loom-worker-session:0.19.1100",
        ]
    );
}

#[test]
fn seat_volume_usage_does_not_measure_after_the_budget_or_fabricate_an_inventory() {
    let mut samples = SeatVolumeSamples::default();
    {
        let _fake = docker::fake::install(Box::new(|_| panic!("budget exhausted")));
        assert_eq!(
            samples.sample_until(Instant::now(), Instant::now, trusted),
            SeatVolumeUsage::default()
        );
    }
    let _fake = docker::fake::install(Box::new(|args| {
        assert!(matches!(args, ["volume", "ls", ..]));
        anyhow::bail!("inventory unavailable")
    }));
    assert_eq!(samples.sample(trusted), SeatVolumeUsage::default());
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
fn seat_volume_usage_rotates_the_recorded_cold_range_into_a_total() {
    // Recorded cold scans across two hosts, including two volumes with no
    // session. Add one deliberately slower attempt to exercise the ceiling.
    const COLD_MILLIS: [u64; 7] = [32_815, 40_030, 47_420, 24_900, 147_200, 147_200, 180_000];
    const REPORTED_KIB: i64 = 39_786_660;
    const SEATS: u32 = 16;
    let start = Instant::now();
    let clock = Rc::new(Cell::new(start));
    let elapsed = Rc::clone(&clock);
    let attempts = Rc::new(Cell::new(0_usize));
    let scan = Rc::clone(&attempts);
    let inject_timeout = Rc::new(Cell::new(false));
    let timeout = Rc::clone(&inject_timeout);
    let _fake = docker::fake::install(Box::new(move |args| {
        elapsed.set(elapsed.get() + Duration::from_millis(18));
        match args {
            ["volume", "ls", ..] => Ok((0..SEATS)
                .map(|n| workspace_volume_name(&format!("seat-{n:02}")))
                .collect::<Vec<_>>()
                .join("\n")),
            ["info", "--format", "{{.DockerRootDir}}"] => Ok(ENGINE_ROOT.into()),
            ["volume", "inspect", "--format", VOLUME_FORMAT, volumes @ ..] => {
                Ok(infos(volumes, CREATED))
            }
            ["run", .., "/usr/bin/timeout", "--", IMAGE, "--kill-after=1s", cap, "/usr/bin/du", "-skx", "--", ROOT] =>
            {
                let n = scan.get();
                scan.set(n + 1);
                let milliseconds = if timeout.get() {
                    COLD_MILLIS[6]
                } else {
                    COLD_MILLIS[n % 6]
                };
                let measured = Duration::from_millis(milliseconds);
                let cap = Duration::from_secs(cap.strip_suffix('s').unwrap().parse().unwrap());
                if cap < measured {
                    elapsed.set(elapsed.get() + cap);
                    anyhow::bail!("modeled cold walk hit the in-container timeout");
                }
                elapsed.set(elapsed.get() + measured);
                Ok(format!("{REPORTED_KIB}\t/workspace\n"))
            }
            unexpected => panic!("unexpected Docker request: {unexpected:?}"),
        }
    }));
    let mut samples = SeatVolumeSamples::default();
    for i in 0..SEATS {
        let tick = start + SEAT_VOLUME_SAMPLE_INTERVAL * i;
        clock.set(tick);
        let result =
            samples.sample_until(tick + SEAT_VOLUME_SAMPLE_INTERVAL, || clock.get(), trusted);
        assert!(clock.get() < tick + SEAT_VOLUME_SAMPLE_INTERVAL);
        assert_eq!(result.seats, [(format!("seat-{i:02}"), REPORTED_KIB * 1024)]);
        assert_eq!(
            result.total,
            (i + 1 == SEATS).then_some(i64::from(SEATS) * REPORTED_KIB * 1024)
        );
    }
    assert_eq!(attempts.get(), SEATS as usize);
    inject_timeout.set(true);
    let tick = start + SEAT_VOLUME_SAMPLE_INTERVAL * SEATS;
    clock.set(tick);
    let result = samples.sample_until(tick + SEAT_VOLUME_SAMPLE_INTERVAL, || clock.get(), trusted);
    assert_eq!(result, SeatVolumeUsage::default());
    assert!(clock.get() < tick + SEAT_VOLUME_SAMPLE_INTERVAL);
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
            Inventory::All => Ok("loom-codex-workspace-alice\nloom-codex-workspace-bob\n".into()),
            Inventory::Alice => Ok("loom-codex-workspace-alice\n".into()),
            Inventory::Bob => Ok("loom-codex-workspace-bob\n".into()),
            Inventory::Missing => anyhow::bail!("cannot list volumes"),
        },
        ["info", "--format", "{{.DockerRootDir}}"] => Ok(ENGINE_ROOT.into()),
        ["volume", "inspect", "--format", VOLUME_FORMAT, volumes @ ..] => {
            Ok(infos(volumes, CREATED))
        }
        ["run", ..] if failed.get() => anyhow::bail!("file vanished during the walk"),
        ["run", ..] => Ok("3\t/workspace\n".into()),
        unexpected => panic!("unexpected Docker request: {unexpected:?}"),
    }));
    let start = Instant::now();
    let mut samples = SeatVolumeSamples::default();
    let mut at = |tick| {
        let now = start + SEAT_VOLUME_SAMPLE_INTERVAL * tick;
        samples.sample_until(now + SEAT_VOLUME_SAMPLE_INTERVAL, || now, trusted)
    };
    assert_eq!(at(0).total, None);
    assert_eq!(at(1).total, Some(6144));
    fail.set(true);
    assert_eq!(at(2), SeatVolumeUsage::default());
    fail.set(false);
    assert_eq!(at(3).total, None);
    assert_eq!(at(4).total, Some(6144));
    assert_eq!(at(6).total, None); // Alice is exactly one roster rotation old.
    assert_eq!(at(7).total, Some(6144));

    inventory.set(Inventory::Bob);
    assert_eq!(at(8).total, Some(3072));
    inventory.set(Inventory::All);
    assert_eq!(at(9).total, Some(6144));

    inventory.set(Inventory::Alice);
    assert_eq!(at(10).total, Some(3072));
    inventory.set(Inventory::All);
    assert_eq!(at(11).total, Some(6144));
    inventory.set(Inventory::Missing);
    assert_eq!(at(12), SeatVolumeUsage::default());
    inventory.set(Inventory::All);
    assert_eq!(at(13).total, None);
}
