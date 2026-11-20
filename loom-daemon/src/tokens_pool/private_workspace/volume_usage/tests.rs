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

fn sample(samples: &mut SeatVolumeSamples) -> SeatVolumeUsage {
    samples.sample_until(Instant::now() + SEAT_VOLUME_SAMPLE_INTERVAL, Instant::now, trusted)
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
        sample(&mut samples),
        SeatVolumeUsage {
            seats: vec![("active".into(), 12_288)],
            total: None
        }
    );
    assert_eq!(
        sample(&mut samples),
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
    assert_eq!(sample(&mut samples), SeatVolumeUsage::default());
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
    assert_eq!(sample(&mut samples).seats, [("alice".to_owned(), 102_400)]);
    *alice_created.borrow_mut() = "2026-11-19T23:35:00Z".into();
    // Bob is measured while Alice's retained 100 KiB describes a replaced volume.
    let bob = sample(&mut samples);
    assert_eq!(
        bob,
        SeatVolumeUsage {
            seats: vec![("bob".into(), 2048)],
            total: None
        }
    );
    let alice = sample(&mut samples);
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
    assert_eq!(reader_image(Some(IMAGE), Instant::now() + SEAT_VOLUME_SAMPLE_INTERVAL), None);
    let _absent = docker::fake::install(Box::new(|args| match args {
        ["image", "inspect", ..] => anyhow::bail!("no such image"),
        unexpected => panic!("pulled or probed an absent image: {unexpected:?}"),
    }));
    assert_eq!(reader_image(Some(IMAGE), Instant::now() + SEAT_VOLUME_SAMPLE_INTERVAL), None);
}

#[test]
fn stock_reader_candidates_include_versioned_tags_newest_first() {
    let _fake = docker::fake::install(Box::new(|args| {
        match args {
        ["image", "ls", "--format", "{{.Repository}}:{{.Tag}}", "ghcr.io/rjwalters/loom-worker-session"] => Ok(
            "ghcr.io/rjwalters/loom-worker-session:0.19.1100\nghcr.io/rjwalters/loom-worker-session:<none>\nghcr.io/rjwalters/loom-worker-session:0.19.1209\nghcr.io/rjwalters/loom-worker-session:latest\nghcr.io/rjwalters/loom-worker-session:foo\n".into(),
        ),
        unexpected => panic!("unexpected Docker request: {unexpected:?}"),
    }
    }));
    assert_eq!(
        local_stock_images(Instant::now() + SEAT_VOLUME_SAMPLE_INTERVAL, &Instant::now),
        [
            "ghcr.io/rjwalters/loom-worker-session:latest",
            "ghcr.io/rjwalters/loom-worker-session:0.19.1209",
            "ghcr.io/rjwalters/loom-worker-session:0.19.1100",
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
    assert_eq!(sample(&mut samples), SeatVolumeUsage::default());
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

#[test]
fn partial_batch_retains_only_confirmed_readings_and_recovers_without_a_rotation() {
    let phase = Rc::new(Cell::new(0));
    let fixture_phase = Rc::clone(&phase);
    let walks = Rc::new(Cell::new(0));
    let fixture_walks = Rc::clone(&walks);
    let _fake = docker::fake::install_timed(Box::new(move |args, _| {
        let phase = fixture_phase.get();
        let reply = match args {
            ["volume", "ls", ..] => ["a", "b", "c", "d", "e", "f"]
                .into_iter()
                .take(if phase == 2 { 3 } else { 6 })
                .map(workspace_volume_name)
                .collect::<Vec<_>>()
                .join("\n"),
            ["volume", "inspect", "--format", VOLUME_FORMAT, volumes @ ..]
                if phase == 1 && volumes.len() > 1 =>
            {
                let mut stdout = infos(&volumes[..3], CREATED);
                // d vanished, e was replaced, and f has a truncated row.
                stdout += &infos(&volumes[4..5], "2026-11-19T23:35:00Z");
                stdout += &format!("{}\tlocal\n", volumes[5]);
                return Ok((false, stdout));
            }
            ["volume", "inspect", "--format", VOLUME_FORMAT, volumes @ ..] => {
                infos(volumes, CREATED)
            }
            ["info", "--format", "{{.DockerRootDir}}"] => ENGINE_ROOT.into(),
            ["run", .., mount, "--entrypoint", _, "--", _, _, _, _, _, _, _] => {
                fixture_walks.set(fixture_walks.get() + 1);
                let blocks = match *mount {
                    mount if mount.contains("-a/") => 1,
                    mount if mount.contains("-b/") => 2,
                    mount if mount.contains("-c/") => 3,
                    _ => 4,
                };
                format!("{blocks}\t/workspace\n")
            }
            unexpected => panic!("unexpected Docker request: {unexpected:?}"),
        };
        Ok((true, reply))
    }));
    let now = Instant::now();
    let deadline = now + SEAT_VOLUME_SAMPLE_INTERVAL;
    let mut samples = SeatVolumeSamples::default();
    for _ in 0..6 {
        samples.sample_until(deadline, || now, trusted);
    }
    assert_eq!(samples.readings.len(), 6);
    let c = samples.readings["c"].clone();

    phase.set(1);
    assert_eq!(
        samples.sample_until(deadline, || now, trusted),
        SeatVolumeUsage {
            seats: vec![("a".into(), 1024)],
            total: None,
        }
    );
    assert_eq!(
        samples
            .readings
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
    assert_eq!(samples.readings["c"], c);

    phase.set(2);
    assert_eq!(
        samples.sample_until(deadline, || now, trusted),
        SeatVolumeUsage {
            seats: vec![("b".into(), 2048)],
            total: Some(6144),
        }
    );
    assert_eq!(walks.get(), 8, "surviving seat c was never re-walked");
}

#[test]
fn incomplete_batch_withholds_total_even_if_the_selected_seat_is_reconfirmed() {
    let batch_status = Rc::new(Cell::new(true));
    let success = Rc::clone(&batch_status);
    let _fake = docker::fake::install_timed(Box::new(move |args, _| match args {
        ["volume", "ls", ..] => Ok((true, workspace_volume_name("alice"))),
        ["volume", "inspect", "--format", VOLUME_FORMAT, volume] => {
            let row = infos(&[volume], CREATED);
            // Batch and single inspect have identical arguments for this
            // roster. Only the first inspect after ls should fail.
            Ok((success.replace(true), row))
        }
        ["info", "--format", "{{.DockerRootDir}}"] => Ok((true, ENGINE_ROOT.into())),
        ["run", ..] => Ok((true, "1\t/workspace\n".into())),
        unexpected => panic!("unexpected Docker request: {unexpected:?}"),
    }));
    let mut samples = SeatVolumeSamples::default();
    assert_eq!(sample(&mut samples).total, Some(1024));
    batch_status.set(false);
    assert_eq!(
        sample(&mut samples),
        SeatVolumeUsage {
            seats: vec![("alice".into(), 1024)],
            total: None,
        }
    );
    assert_eq!(sample(&mut samples).total, Some(1024));
}

#[test]
fn reader_selection_shares_one_deadline_across_local_probes_and_bundle_checks() {
    let seconds = Duration::from_secs;
    let start = Instant::now();
    let deadline = start + seconds(8);
    let clock = Rc::new(Cell::new(start));
    let elapsed = Rc::clone(&clock);
    let probes = Rc::new(Cell::new(0));
    let fixture_probes = Rc::clone(&probes);
    let _fake = docker::fake::install_timed(Box::new(move |args, timeout| {
        let reply = match args {
            ["volume", "ls", ..] => workspace_volume_name("alice"),
            ["image", "ls", ..] => {
                assert_eq!(timeout, seconds(3));
                elapsed.set(elapsed.get() + seconds(2));
                format!(
                    "{DEFAULT_SESSION_IMAGE}\nghcr.io/rjwalters/loom-worker-session:0.19.2\nghcr.io/rjwalters/loom-worker-session:0.19.1\n"
                )
            }
            ["image", "inspect", "--format", "{{.Id}}", image] => {
                assert_ne!(*image, "ghcr.io/rjwalters/loom-worker-session:0.19.1");
                assert_eq!(timeout, seconds(3));
                elapsed.set(elapsed.get() + seconds(1));
                "sha256:0\n".into()
            }
            ["run", .., "--entrypoint", "/bin/sh", image, "-c", _] => {
                fixture_probes.set(fixture_probes.get() + 1);
                if *image == DEFAULT_SESSION_IMAGE {
                    assert_eq!(timeout, seconds(5));
                    elapsed.set(elapsed.get() + seconds(2));
                    "__LOOM_BUNDLE_NO_MANIFEST__\n".into()
                } else {
                    assert_eq!(*image, "ghcr.io/rjwalters/loom-worker-session:0.19.2");
                    assert_eq!(timeout, seconds(2));
                    elapsed.set(elapsed.get() + timeout);
                    anyhow::bail!("modeled bundle timeout");
                }
            }
            unexpected => panic!("Docker request after reader deadline: {unexpected:?}"),
        };
        Ok((true, reply))
    }));
    let mut samples = SeatVolumeSamples::default();
    assert_eq!(
        samples.sample_until(
            deadline,
            || clock.get(),
            || { reader_image_until(None, deadline, &|| clock.get()) }
        ),
        SeatVolumeUsage::default()
    );
    assert_eq!(probes.get(), 2);
    assert_eq!(clock.get(), deadline);
}

#[test]
fn reader_selection_caps_listing_and_presence_by_the_remaining_budget() {
    let start = Instant::now();
    let duration = Duration::from_millis(500);
    let deadline = start + duration;
    let clock = Rc::new(Cell::new(start));
    let elapsed = Rc::clone(&clock);
    let _fake = docker::fake::install_timed(Box::new(move |args, timeout| {
        assert!(matches!(args, ["image", "ls", ..] | ["image", "inspect", ..]));
        assert_eq!(timeout, duration);
        elapsed.set(deadline);
        Ok((true, DEFAULT_SESSION_IMAGE.into()))
    }));
    assert_eq!(reader_image_until(None, deadline, &|| clock.get()), None);
    clock.set(start);
    assert_eq!(reader_image_until(Some(IMAGE), deadline, &|| clock.get()), None);
}

#[test]
fn configuration_failure_preserves_only_readings_still_fresh_on_recovery() {
    for recovery in [Duration::from_secs(40), SEAT_VOLUME_SAMPLE_INTERVAL * 2] {
        let config_valid = Rc::new(Cell::new(true));
        let valid = Rc::clone(&config_valid);
        let _fake = docker::fake::install(Box::new(move |args| {
            assert!(valid.get(), "invalid config must not invoke Docker");
            match args {
                ["volume", "ls", ..] => {
                    Ok("loom-codex-workspace-alice\nloom-codex-workspace-bob\n".into())
                }
                ["volume", "inspect", "--format", VOLUME_FORMAT, volumes @ ..] => {
                    Ok(infos(volumes, CREATED))
                }
                ["info", "--format", "{{.DockerRootDir}}"] => Ok(ENGINE_ROOT.into()),
                ["run", ..] => Ok("1\t/workspace\n".into()),
                unexpected => panic!("unexpected Docker request: {unexpected:?}"),
            }
        }));
        let start = Instant::now();
        let mut samples = SeatVolumeSamples::default();
        let mut pass = |now, config| {
            samples.sample_configured_until(
                config,
                now + SEAT_VOLUME_SAMPLE_INTERVAL,
                || now,
                |_, _| trusted(),
            )
        };
        assert_eq!(
            pass(start, Ok(SessionImageConfig::default()))
                .unwrap()
                .total,
            None
        );
        config_valid.set(false);
        assert_eq!(
            pass(start + Duration::from_secs(20), Err(anyhow::anyhow!("invalid configuration"))),
            None
        );
        config_valid.set(true);
        let after = pass(start + recovery, Ok(SessionImageConfig::default())).unwrap();
        assert_eq!(after.seats, [("bob".into(), 1024)]);
        if recovery < SEAT_VOLUME_SAMPLE_INTERVAL * 2 {
            assert_eq!(after.total, Some(2048));
        } else {
            assert_eq!(after.total, None);
            let next = pass(
                start + recovery + SEAT_VOLUME_SAMPLE_INTERVAL,
                Ok(SessionImageConfig::default()),
            )
            .unwrap();
            assert_eq!(next.seats, [("alice".into(), 1024)]);
            assert_eq!(next.total, Some(2048));
        }
    }
}

#[test]
fn permanently_unmeasurable_prefix_volume_never_looks_like_a_complete_roster() {
    let _fake = docker::fake::install(Box::new(|args| match args {
        ["volume", "ls", ..] => {
            Ok("loom-codex-workspace-alice\nloom-codex-workspace-legacy\n".into())
        }
        ["volume", "inspect", "--format", VOLUME_FORMAT, volumes @ ..] => Ok(volumes
            .iter()
            .map(|volume| {
                let account = volume.strip_prefix(VOLUME_PREFIX).unwrap();
                // The prefix-matching legacy volume has no loom.account owner.
                volume_info(volume, if account == "legacy" { "" } else { account }, 0, CREATED)
            })
            .collect()),
        ["info", "--format", "{{.DockerRootDir}}"] => Ok(ENGINE_ROOT.into()),
        ["run", .., mount, "--entrypoint", _, "--", _, _, _, _, _, _, _] => {
            assert!(mount.contains("-alice/"));
            Ok("1\t/workspace\n".into())
        }
        unexpected => panic!("unexpected Docker request: {unexpected:?}"),
    }));
    let mut samples = SeatVolumeSamples::default();
    for _ in 0..3 {
        assert_eq!(
            sample(&mut samples),
            SeatVolumeUsage {
                seats: vec![("alice".into(), 1024)],
                total: None
            }
        );
        assert_eq!(sample(&mut samples), SeatVolumeUsage::default());
    }
}
