use std::cell::Cell;

use super::*;

fn valid(v: &str) -> FloorRead {
    FloorRead::Valid {
        version: v.to_string(),
        source: "fleet/fleet.json",
    }
}

fn release(v: &str) -> Release {
    Release {
        tag: format!("v{v}"),
        version: v.to_string(),
    }
}

/// A source whose newest release and fetch outcome are scripted. `fetch`
/// writes `payload` to a file when it succeeds.
struct Fake {
    newest: Result<Release, String>,
    /// `Ok(bytes)` is a verified download; `Err` a fetch/checksum failure.
    fetch: Result<Vec<u8>, String>,
    dir: PathBuf,
    fetched: Cell<u32>,
}

impl Source for Fake {
    fn newest(&self) -> Result<Release> {
        self.newest.clone().map_err(|e| anyhow!(e))
    }
    fn fetch(&self, _release: &Release) -> Result<PathBuf> {
        self.fetched.set(self.fetched.get() + 1);
        let bytes = self.fetch.clone().map_err(|e| anyhow!(e))?;
        let p = self.dir.join("downloaded");
        std::fs::write(&p, bytes)?;
        Ok(p)
    }
}

struct Rig {
    _tmp: tempfile::TempDir,
    dest: PathBuf,
    fake: Fake,
}

fn rig(newest: Result<Release, String>, fetch: Result<Vec<u8>, String>) -> Rig {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let scratch = tmp.path().join("scratch");
    std::fs::create_dir(&scratch).unwrap();
    let dest = bin.join("loom-daemon");
    std::fs::write(&dest, b"#!/bin/sh\necho old\n").unwrap();
    Rig {
        dest,
        fake: Fake {
            newest,
            fetch,
            dir: scratch,
            fetched: Cell::new(0),
        },
        _tmp: tmp,
    }
}

const OLD: &[u8] = b"#!/bin/sh\necho old\n";
const NEW: &[u8] = b"#!/bin/sh\necho new\n";

#[test]
fn below_floor_installs_and_keeps_previous() {
    let r = rig(Ok(release("0.19.1200")), Ok(NEW.to_vec()));
    let code = apply(&valid("0.19.1196"), "0.19.1072", &r.dest, false, &r.fake);
    assert_eq!(code, 0);
    assert_eq!(std::fs::read(&r.dest).unwrap(), NEW);
    let prev = r.dest.with_file_name("loom-daemon.previous");
    assert_eq!(std::fs::read(prev).unwrap(), OLD);
}

#[test]
fn at_floor_is_a_noop_that_downloads_nothing() {
    let r = rig(Ok(release("0.19.1200")), Ok(NEW.to_vec()));
    let code = apply(&valid("0.19.1196"), "0.19.1196", &r.dest, false, &r.fake);
    assert_eq!(code, 0);
    assert_eq!(r.fake.fetched.get(), 0);
    assert_eq!(std::fs::read(&r.dest).unwrap(), OLD);
}

#[test]
fn above_floor_is_a_noop() {
    let r = rig(Ok(release("0.19.1200")), Ok(NEW.to_vec()));
    let code = apply(&valid("0.19.1196"), "0.19.1300", &r.dest, false, &r.fake);
    assert_eq!(code, 0);
    assert_eq!(r.fake.fetched.get(), 0);
    assert_eq!(std::fs::read(&r.dest).unwrap(), OLD);
}

#[test]
fn no_floor_is_a_noop() {
    let r = rig(Ok(release("0.19.1200")), Ok(NEW.to_vec()));
    assert_eq!(apply(&FloorRead::Absent, "0.19.1", &r.dest, false, &r.fake), 0);
    assert_eq!(r.fake.fetched.get(), 0);
}

#[test]
fn malformed_floor_changes_nothing() {
    let r = rig(Ok(release("0.19.1200")), Ok(NEW.to_vec()));
    let read = FloorRead::Malformed {
        source: "fleet/fleet.json",
        detail: "not canonical".to_string(),
    };
    assert_eq!(apply(&read, "0.19.1", &r.dest, false, &r.fake), 2);
    assert_eq!(r.fake.fetched.get(), 0);
    assert_eq!(std::fs::read(&r.dest).unwrap(), OLD);
}

#[test]
fn fetch_failure_keeps_the_old_binary() {
    let r = rig(Ok(release("0.19.1200")), Err("download failed".to_string()));
    let code = apply(&valid("0.19.1196"), "0.19.1072", &r.dest, false, &r.fake);
    assert_eq!(code, 2);
    assert_eq!(std::fs::read(&r.dest).unwrap(), OLD);
    assert!(!r.dest.with_file_name("loom-daemon.previous").exists());
}

#[test]
fn checksum_mismatch_keeps_the_old_binary() {
    let r = rig(
        Ok(release("0.19.1200")),
        Err("verification of release v0.19.1200 FAILED (checksum or signature)".to_string()),
    );
    let code = apply(&valid("0.19.1196"), "0.19.1072", &r.dest, false, &r.fake);
    assert_eq!(code, 2);
    assert_eq!(std::fs::read(&r.dest).unwrap(), OLD);
    assert!(!r.dest.with_file_name("loom-daemon.previous").exists());
}

#[test]
fn unsatisfiable_floor_changes_nothing() {
    // Newest release (0.19.1100) is still under the floor.
    let r = rig(Ok(release("0.19.1100")), Ok(NEW.to_vec()));
    let code = apply(&valid("0.19.1196"), "0.19.1072", &r.dest, false, &r.fake);
    assert_eq!(code, 2);
    assert_eq!(r.fake.fetched.get(), 0);
    assert_eq!(std::fs::read(&r.dest).unwrap(), OLD);
}

#[test]
fn unresolvable_release_changes_nothing() {
    let r = rig(Err("no releases".to_string()), Ok(NEW.to_vec()));
    let code = apply(&valid("0.19.1196"), "0.19.1072", &r.dest, false, &r.fake);
    assert_eq!(code, 2);
    assert_eq!(std::fs::read(&r.dest).unwrap(), OLD);
}

#[test]
fn dry_run_writes_and_downloads_nothing() {
    let r = rig(Ok(release("0.19.1200")), Ok(NEW.to_vec()));
    let code = apply(&valid("0.19.1196"), "0.19.1072", &r.dest, true, &r.fake);
    assert_eq!(code, 0);
    assert_eq!(r.fake.fetched.get(), 0);
    assert_eq!(std::fs::read(&r.dest).unwrap(), OLD);
}

#[test]
fn drift_line_only_when_below_floor() {
    let snap = |floor: &str| {
        let mut files = std::collections::BTreeMap::new();
        files.insert(
            store::ROSTER_PATH.to_string(),
            format!("loom_min_version: \"{floor}\"\nrepos: []\n").into_bytes(),
        );
        Snapshot {
            manifest: fetch::Manifest {
                version: 1,
                repo: "o/r".to_string(),
                reference: "main".to_string(),
                commit: "abc".to_string(),
                etag: None,
                confirmed_at: Utc::now(),
                files: std::collections::BTreeMap::new(),
            },
            files,
        }
    };
    let below = cli_below_floor_line(&snap("0.19.1196"), "0.19.1072").unwrap();
    assert!(
        below.contains("CLI below floor: installed 0.19.1072, floor 0.19.1196"),
        "{below}"
    );
    assert!(cli_below_floor_line(&snap("0.19.1196"), "0.19.1196").is_none());
    assert!(cli_below_floor_line(&snap("0.19.1196"), "0.19.1300").is_none());
}
