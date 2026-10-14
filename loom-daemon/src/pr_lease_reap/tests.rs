//! Tests for the timed-out run's PR-lease reap (#11931).

use super::*;
use std::os::unix::fs::PermissionsExt;

fn judge(pr: u64, sweep: &str) -> Held {
    Held {
        pr,
        role: "judge".into(),
        host: "host-a1b2c3d4".into(),
        sweep: sweep.into(),
    }
}

#[test]
fn a_held_line_round_trips_and_malformed_lines_drop() {
    let h = judge(77, "sweep-1");
    assert_eq!(Held::parse(&h.line()), Some(h.clone()));
    for bad in [
        "",
        "77 judge host",
        "77 builder host sweep",
        "x judge host sweep",
        "77 judge host sweep extra",
    ] {
        assert_eq!(Held::parse(bad), None, "{bad:?}");
    }
    let text = format!("{}\nnoise\n{}\n", h.line(), h.line());
    assert_eq!(parse_file(&text), vec![h], "duplicates and noise drop");
}

#[test]
fn claim_lists_a_lease_once_and_release_drops_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("state/run.leases");
    let a = judge(77, "s1");
    let b = judge(78, "s1");
    record_in(&path, &a);
    record_in(&path, &a);
    record_in(&path, &b);
    let read = || parse_file(&std::fs::read_to_string(&path).unwrap());
    assert_eq!(read(), vec![a.clone(), b.clone()]);
    forget_in(&path, &a);
    assert_eq!(read(), vec![b]);
    // A field that would not round-trip is never written.
    let bad = Held {
        sweep: "two words".into(),
        ..judge(79, "x")
    };
    record_in(&path, &bad);
    assert_eq!(read().len(), 1);
}

#[test]
fn the_reaper_hands_the_child_its_file_clears_stale_ones_and_removes_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let stale = path_for(dir.path(), "role-judge-1");
    std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
    std::fs::write(&stale, judge(1, "old").line()).unwrap();
    let reaper = Reaper::plan(dir.path(), "role-judge-1");
    assert!(!stale.exists(), "an earlier run's file is cleared");
    let mut cmd = Command::new("true");
    reaper.apply(&mut cmd);
    let env: Vec<_> = cmd.get_envs().collect();
    assert!(env
        .iter()
        .any(|(k, v)| *k == LEASE_FILE_ENV && *v == Some(reaper.path().as_os_str())));
    record_in(reaper.path(), &judge(77, "s1"));
    let path = reaper.path().to_path_buf();
    assert!(path.exists());
    drop(reaper);
    assert!(!path.exists(), "the run's file goes with the run");
    // An item name never escapes the state directory.
    assert!(path_for(dir.path(), "../x/y").starts_with(dir.path().join(".loom/state")));
}

#[test]
fn reap_runs_the_reap_verb_once_per_listed_lease() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("argv");
    let bin = dir.path().join("fake-daemon");
    std::fs::write(&bin, format!("#!/bin/sh\necho \"$*\" >> '{}'\n", log.display())).unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let reaper = Reaper::plan(dir.path(), "role-judge-2");
    // Nothing listed: nothing run.
    assert_eq!(reaper.reap(&bin, REASON_TIMEOUT, |_| {}), 0);
    assert!(!log.exists());
    record_in(reaper.path(), &judge(77, "s1"));
    record_in(reaper.path(), &judge(78, "s1"));
    let ok = reaper.reap(&bin, REASON_TIMEOUT, |cmd| {
        cmd.env("MARK", "1");
    });
    assert_eq!(ok, 2);
    let argv = std::fs::read_to_string(&log).unwrap();
    let lines: Vec<&str> = argv.lines().collect();
    let root = dir.path().display();
    assert_eq!(
        lines,
        vec![
            format!(
                "lease pr reap 77 --role judge --host host-a1b2c3d4 --sweep-id s1 --reason \
                 timeout --workspace {root}"
            ),
            format!(
                "lease pr reap 78 --role judge --host host-a1b2c3d4 --sweep-id s1 --reason \
                 timeout --workspace {root}"
            ),
        ]
    );
}

#[test]
fn a_failing_reap_is_not_counted() {
    let dir = tempfile::tempdir().unwrap();
    let reaper = Reaper::plan(dir.path(), "role-judge-3");
    record_in(reaper.path(), &judge(77, "s1"));
    assert_eq!(reaper.reap(Path::new("/bin/false"), REASON_TIMEOUT, |_| {}), 0);
    assert_eq!(reaper.reap(&dir.path().join("missing"), REASON_TIMEOUT, |_| {}), 0);
}
