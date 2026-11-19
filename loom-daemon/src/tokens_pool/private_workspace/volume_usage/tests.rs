use super::*;

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
            ["exec", "--user", "1000:1000", ALICE_ID, "/usr/bin/timeout", "--kill-after=1s", "8s", "/usr/bin/du", "-skx", "--", ROOT] => {
                "12\t/workspace\n".into()
            }
            ["exec", "--user", "1000:1000", BOB_ID, "/usr/bin/timeout", "--kill-after=1s", "8s", "/usr/bin/du", "-skx", "--", ROOT] => {
                "0\t/workspace\n".into()
            }
            unexpected => panic!("unexpected Docker request: {unexpected:?}"),
        };
        Ok(reply)
    }));
    assert_eq!(
        sample_seat_volumes(),
        SeatVolumeUsage {
            seats: vec![("alice".into(), 12_288), ("bob".into(), 0)],
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
    assert_eq!(
        sample_seat_volumes(),
        SeatVolumeUsage {
            seats: vec![("alice".into(), 3072)],
            total: None
        }
    );
    // Only the two verified mounts reached exec, even though one read failed.
    assert_eq!(docker::fake::seen_env().len(), 15);
}

#[test]
fn seat_volume_usage_does_not_measure_after_the_budget_or_fabricate_an_inventory() {
    {
        let _fake = docker::fake::install(Box::new(|_| panic!("budget exhausted")));
        assert_eq!(sample_until(Instant::now()), SeatVolumeUsage::default());
    }
    let _fake = docker::fake::install(Box::new(|args| {
        assert!(matches!(args, ["volume", "ls", ..]));
        anyhow::bail!("inventory unavailable")
    }));
    assert_eq!(sample_seat_volumes(), SeatVolumeUsage::default());
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
