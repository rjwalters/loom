//! Tests for the retained previous binary and the install record (#10983).
//!
//! Every test works in its own tempdir; none touches a real daemon
//! destination, supervisor or state dir.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::super::{install_to, publish, stage};
use super::{
    begin, previous_path, read_record, record_path, InstallError, InstallRecord, Phase, RecordRead,
    SCHEMA_VERSION, UNKNOWN_VERSION,
};

fn tmpdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("loom-install-txn-")
        .tempdir()
        .unwrap()
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// The body of a stand-in daemon that answers `--version` with `version`.
fn stub_body(version: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho \"loom-daemon {version} (commit abc1234)\"\n").into_bytes()
}

fn write_exec(path: &Path, body: &[u8]) {
    std::fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// A tempdir holding `loom-daemon` (when `live` is given) plus a second
/// tempdir for candidates, so candidates never show up beside the destination.
struct Fixture {
    dir: tempfile::TempDir,
    src: tempfile::TempDir,
    dest: PathBuf,
}

impl Fixture {
    fn new(live: Option<&[u8]>) -> Self {
        let dir = tmpdir();
        let dest = dir.path().join("loom-daemon");
        if let Some(body) = live {
            write_exec(&dest, body);
        }
        Self {
            dir,
            src: tmpdir(),
            dest,
        }
    }

    fn candidate(&self, name: &str, body: &[u8]) -> PathBuf {
        let p = self.src.path().join(name);
        write_exec(&p, body);
        p
    }

    /// Hidden entries beside the destination: staging residue, if any.
    fn hidden(&self) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        names.sort();
        names
    }

    fn record(&self) -> InstallRecord {
        match read_record(&self.dest) {
            RecordRead::Known(r) => r,
            other => panic!("expected a known record, got {other:?}"),
        }
    }

    fn previous(&self) -> Vec<u8> {
        std::fs::read(previous_path(&self.dest)).unwrap()
    }
}

#[cfg(unix)]
fn ino(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).unwrap().ino()
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
}

#[cfg(unix)]
fn set_mode(p: &Path, m: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
}

#[cfg(unix)]
fn running_as_root() -> bool {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// `<path> --version`, retried past the multi-threaded harness's ETXTBSY race.
#[cfg(unix)]
fn answers(path: &Path) -> String {
    for _ in 0..100 {
        match std::process::Command::new(path).arg("--version").output() {
            Ok(o) => return String::from_utf8_lossy(&o.stdout).trim().to_string(),
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("could not run {}: {e}", path.display()),
        }
    }
    panic!("{} stayed ETXTBSY", path.display());
}

// ---------------------------------------------------------------------------
// Retention
// ---------------------------------------------------------------------------

/// Acceptance 1: after an install over an existing binary, `.previous` is
/// byte-identical to what was live, and its sha256 and version are recorded.
#[cfg(unix)]
#[test]
fn an_install_keeps_the_previous_binary_with_its_sha256_and_version() {
    let old = stub_body("0.0.1");
    let new = stub_body("0.0.2");
    let fx = Fixture::new(Some(&old));
    let fresh = fx.candidate("fresh", &new);

    assert!(install_to(&fresh, &fx.dest));

    assert_eq!(std::fs::read(&fx.dest).unwrap(), new);
    assert_eq!(fx.previous(), old, "the retained copy must be byte-identical");
    assert_eq!(mode(&previous_path(&fx.dest)), 0o755);
    assert!(answers(&previous_path(&fx.dest)).contains("0.0.1"), "the copy must run");

    let r = fx.record();
    assert_eq!(r.schema_version, SCHEMA_VERSION);
    assert_eq!(r.phase, Phase::Committed);
    assert_eq!(r.dest, fx.dest.display().to_string());
    assert_eq!(r.target.sha256, sha(&new));
    assert_eq!(r.target.version, "loom-daemon 0.0.2 (commit abc1234)");
    let kept = r.previous.expect("an upgrade records what it replaced");
    assert_eq!(kept.sha256, sha(&old));
    assert_eq!(kept.version, "loom-daemon 0.0.1 (commit abc1234)");
    assert_eq!(kept.path, previous_path(&fx.dest).display().to_string());
    assert!(r.updated_at >= r.started_at);
    assert_eq!(fx.hidden(), Vec::<String>::new(), "no staging residue");
}

/// Acceptance 5: a first-ever install succeeds with no previous copy.
#[test]
fn a_first_ever_install_succeeds_with_no_previous_copy() {
    let fx = Fixture::new(None);
    let fresh = fx.candidate("fresh", &stub_body("0.0.1"));

    assert!(install_to(&fresh, &fx.dest));

    assert!(!previous_path(&fx.dest).exists());
    let r = fx.record();
    assert_eq!(r.phase, Phase::Committed);
    assert_eq!(r.previous, None);
    assert_eq!(fx.hidden(), Vec::<String>::new());
}

/// Exactly one previous binary is kept: each install replaces the last one's.
#[test]
fn only_the_most_recent_previous_binary_is_kept() {
    let (a, b, c) = (stub_body("1"), stub_body("2"), stub_body("3"));
    let fx = Fixture::new(Some(&a));

    assert!(install_to(&fx.candidate("b", &b), &fx.dest));
    assert_eq!(fx.previous(), a);
    assert!(install_to(&fx.candidate("c", &c), &fx.dest));

    assert_eq!(fx.previous(), b);
    assert_eq!(fx.record().previous.unwrap().sha256, sha(&b));
    let kept = std::fs::read_dir(fx.dir.path())
        .unwrap()
        .filter(|e| {
            let name = e.as_ref().unwrap().file_name();
            name.to_string_lossy().contains("previous")
        })
        .count();
    assert_eq!(kept, 1);
}

/// Re-running an install of the bytes already live must not rotate the
/// candidate into `.previous`: that would destroy the only recovery copy.
#[test]
fn reinstalling_the_live_bytes_keeps_the_existing_recovery_copy() {
    let (a, b) = (stub_body("1"), stub_body("2"));
    let fx = Fixture::new(Some(&a));
    let fresh = fx.candidate("b", &b);
    assert!(install_to(&fresh, &fx.dest));

    assert!(install_to(&fresh, &fx.dest));

    assert_eq!(std::fs::read(&fx.dest).unwrap(), b);
    assert_eq!(fx.previous(), a, "the recovery copy must still be the older binary");
    let r = fx.record();
    assert_eq!(r.phase, Phase::Committed);
    assert_eq!(r.previous.unwrap().sha256, sha(&a));
}

/// The same, with nothing retained yet: no copy is invented.
#[test]
fn reinstalling_the_live_bytes_with_no_recovery_copy_records_none() {
    let a = stub_body("1");
    let fx = Fixture::new(Some(&a));

    assert!(install_to(&fx.candidate("a", &a), &fx.dest));

    assert!(!previous_path(&fx.dest).exists());
    assert_eq!(fx.record().previous, None);
}

/// `.previous` is itself replaced by rename, never rewritten in place: a
/// daemon that has not restarted since the last install may be executing it.
#[cfg(unix)]
#[test]
fn the_retained_copy_is_replaced_by_rename_not_rewritten() {
    use std::io::Read;

    let (a, b, c) = (stub_body("1"), stub_body("2"), stub_body("3"));
    let fx = Fixture::new(Some(&a));
    assert!(install_to(&fx.candidate("b", &b), &fx.dest));
    let keep = previous_path(&fx.dest);
    let before = ino(&keep);
    let mut held = std::fs::File::open(&keep).unwrap();

    assert!(install_to(&fx.candidate("c", &c), &fx.dest));

    assert_ne!(before, ino(&keep));
    let mut still = Vec::new();
    held.read_to_end(&mut still).unwrap();
    assert_eq!(still, a, "the old retained inode must be left intact");
}

/// A live binary that does not answer `--version` is still kept, and still
/// replaced: installing over a broken binary is a repair.
#[test]
fn a_live_binary_with_no_version_is_kept_as_unknown() {
    let fx = Fixture::new(Some(b"not a program"));
    let fresh = fx.candidate("fresh", &stub_body("2"));

    assert!(install_to(&fresh, &fx.dest));

    assert_eq!(fx.previous(), b"not a program");
    assert_eq!(fx.record().previous.unwrap().version, UNKNOWN_VERSION);
}

/// A symlinked destination: what is kept is the binary the link resolved to,
/// which is what was live.
#[cfg(unix)]
#[test]
fn a_symlinked_destination_keeps_the_binary_it_pointed_at() {
    let old = stub_body("1");
    let fx = Fixture::new(None);
    let real = fx.src.path().join("real");
    write_exec(&real, &old);
    std::os::unix::fs::symlink(&real, &fx.dest).unwrap();

    assert!(install_to(&fx.candidate("fresh", &stub_body("2")), &fx.dest));

    assert_eq!(fx.previous(), old);
    assert!(std::fs::symlink_metadata(previous_path(&fx.dest))
        .unwrap()
        .file_type()
        .is_file());
    assert_eq!(std::fs::read(&real).unwrap(), old);
}

/// macOS code signing: the retained copy of a REAL signed executable must
/// still launch. A copy of this test binary stands in for the daemon (a
/// copied Apple platform binary is killed at launch wherever it lives, see
/// `replaces_a_currently_executing_binary`); on macOS it carries the linker's
/// ad-hoc signature, which a byte-identical copy keeps. Nothing re-signs it.
#[cfg(unix)]
#[test]
fn the_retained_copy_of_a_real_executable_still_runs() {
    let me = std::env::current_exe().unwrap();
    let fx = Fixture::new(None);
    std::fs::copy(&me, &fx.dest).unwrap();
    set_mode(&fx.dest, 0o755);

    assert!(install_to(&fx.candidate("fresh", &stub_body("2")), &fx.dest));

    let keep = previous_path(&fx.dest);
    let r = fx.record();
    let kept = r.previous.unwrap();
    let mut original = std::fs::File::open(&me).unwrap();
    assert_eq!(
        kept.sha256,
        super::copy_hashing(&mut original, &mut std::io::sink()).unwrap(),
        "the retained copy must be byte-identical to the executable it replaced"
    );
    // Run it: a filter that matches nothing makes the harness exit 0 at once.
    let mut status = None;
    for _ in 0..100 {
        let run = std::process::Command::new(&keep)
            .args(["--exact", "no::such::test::in::this::binary"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match run {
            Ok(s) => {
                status = Some(s);
                break;
            }
            Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(e) => panic!("the retained copy could not be launched: {e}"),
        }
    }
    let status = status.expect("the retained copy stayed ETXTBSY");
    assert!(status.success(), "the retained copy did not run: {status:?}");
}

// ---------------------------------------------------------------------------
// Refusal
// ---------------------------------------------------------------------------

/// Acceptance 4: when the previous binary cannot be kept, the install fails
/// before the live path is touched. Here `<dest>.previous` is a non-empty
/// directory, so the copy is written and its rename into place fails.
#[cfg(unix)]
#[test]
fn an_install_that_cannot_keep_the_previous_binary_is_refused() {
    let old = stub_body("1");
    let fx = Fixture::new(Some(&old));
    let keep = previous_path(&fx.dest);
    std::fs::create_dir(&keep).unwrap();
    std::fs::write(keep.join("in-the-way"), b"x").unwrap();
    let before = ino(&fx.dest);
    let fresh = fx.candidate("fresh", &stub_body("2"));

    let staged = stage(&fresh, &fx.dest).unwrap();
    let err = publish(&staged, &fx.dest).unwrap_err();

    assert!(matches!(err, InstallError::Retain { .. }), "got {err:?}");
    let said = err.to_string();
    assert!(said.contains("Refusing to install"), "{said}");
    assert!(said.contains(&keep.display().to_string()), "{said}");
    assert_eq!(std::fs::read(&fx.dest).unwrap(), old);
    assert_eq!(before, ino(&fx.dest), "the live binary must not have been replaced");
    assert!(answers(&fx.dest).contains("loom-daemon 1 "));
    assert_eq!(read_record(&fx.dest), RecordRead::Absent);
    assert_eq!(fx.hidden(), Vec::<String>::new(), "the staged file must be removed");
    assert!(!install_to(&fresh, &fx.dest), "install_to reports the same refusal");
    assert_eq!(std::fs::read(&fx.dest).unwrap(), old);
}

/// The same refusal when the live binary cannot be read at all.
#[cfg(unix)]
#[test]
fn an_unreadable_live_binary_refuses_the_install() {
    if running_as_root() {
        return;
    }
    let old = stub_body("1");
    let fx = Fixture::new(Some(&old));
    set_mode(&fx.dest, 0o000);
    let before = ino(&fx.dest);

    let staged = stage(&fx.candidate("fresh", &stub_body("2")), &fx.dest).unwrap();
    let err = publish(&staged, &fx.dest).unwrap_err();

    set_mode(&fx.dest, 0o755);
    assert!(matches!(err, InstallError::Retain { .. }), "got {err:?}");
    assert_eq!(before, ino(&fx.dest));
    assert_eq!(std::fs::read(&fx.dest).unwrap(), old);
    assert!(!previous_path(&fx.dest).exists());
    assert_eq!(fx.hidden(), Vec::<String>::new());
}

/// No record, no install: a record that cannot be written refuses too.
#[cfg(unix)]
#[test]
fn an_install_whose_record_cannot_be_written_is_refused() {
    let old = stub_body("1");
    let fx = Fixture::new(Some(&old));
    let rec = record_path(&fx.dest);
    std::fs::create_dir(&rec).unwrap();
    std::fs::write(rec.join("in-the-way"), b"x").unwrap();
    let before = ino(&fx.dest);

    let staged = stage(&fx.candidate("fresh", &stub_body("2")), &fx.dest).unwrap();
    let err = publish(&staged, &fx.dest).unwrap_err();

    assert!(matches!(err, InstallError::Record { .. }), "got {err:?}");
    assert_eq!(before, ino(&fx.dest));
    assert_eq!(std::fs::read(&fx.dest).unwrap(), old);
    assert_eq!(fx.hidden(), Vec::<String>::new());
}

/// `publish` only renames a file `stage` made for that destination, and does
/// not delete one that is not.
#[test]
fn publish_refuses_a_file_that_was_not_staged_for_the_destination() {
    let old = stub_body("1");
    let fx = Fixture::new(Some(&old));
    let stray = fx.candidate("fresh", &stub_body("2"));
    let beside = fx.dir.path().join("fresh");
    write_exec(&beside, &stub_body("2"));

    for not_staged in [&stray, &beside] {
        let err = publish(not_staged, &fx.dest).unwrap_err();
        assert!(matches!(err, InstallError::NotStaged(_)), "got {err:?}");
        assert!(not_staged.exists(), "a file that is not ours must not be removed");
    }
    assert_eq!(std::fs::read(&fx.dest).unwrap(), old);
    assert!(!previous_path(&fx.dest).exists());
}

// ---------------------------------------------------------------------------
// Interruption
// ---------------------------------------------------------------------------

/// Acceptance 3: an install killed between staging and publishing leaves the
/// live binary untouched and runnable. The kill is the absence of the
/// `publish` call: nothing runs after `stage` returns.
#[cfg(unix)]
#[test]
fn a_kill_between_staging_and_publishing_leaves_the_live_binary_untouched() {
    let old = stub_body("0.0.1");
    let new = stub_body("0.0.2");
    let fx = Fixture::new(Some(&old));
    let before = ino(&fx.dest);
    let fresh = fx.candidate("fresh", &new);

    let staged = stage(&fresh, &fx.dest).unwrap();

    assert_eq!(std::fs::read(&fx.dest).unwrap(), old);
    assert_eq!(before, ino(&fx.dest));
    assert_eq!(mode(&fx.dest), 0o755);
    assert!(answers(&fx.dest).contains("0.0.1"), "the live binary must still run");
    assert!(!previous_path(&fx.dest).exists());
    assert_eq!(read_record(&fx.dest), RecordRead::Absent);
    assert_eq!(std::fs::read(&staged).unwrap(), new, "the staged file is complete");
    assert_eq!(staged.parent(), fx.dest.parent(), "staged beside the destination");

    // A later install is not disturbed by the orphan.
    assert!(install_to(&fresh, &fx.dest));
    assert_eq!(std::fs::read(&fx.dest).unwrap(), new);
    assert_eq!(fx.previous(), old);
}

/// The other side of that window: killed after the recovery copy and the
/// `staged` record are in place but before the rename. The live binary is
/// still the old one, and the record says exactly that.
#[cfg(unix)]
#[test]
fn a_kill_after_the_staged_record_leaves_the_live_binary_untouched() {
    let old = stub_body("0.0.1");
    let new = stub_body("0.0.2");
    let fx = Fixture::new(Some(&old));
    let before = ino(&fx.dest);
    let fresh = fx.candidate("fresh", &new);

    let staged = stage(&fresh, &fx.dest).unwrap();
    let record = begin(&staged, &fx.dest).unwrap();

    assert_eq!(record.phase, Phase::Staged);
    assert_eq!(fx.record(), record, "what is on disk is what was returned");
    assert_eq!(std::fs::read(&fx.dest).unwrap(), old);
    assert_eq!(before, ino(&fx.dest));
    assert!(answers(&fx.dest).contains("0.0.1"));
    // The invariant a later slice relies on: a record implies the copy.
    assert_eq!(fx.previous(), old);
    assert_eq!(record.previous.unwrap().sha256, sha(&old));
    assert_eq!(record.target.sha256, sha(&new));

    // The next install runs over the unfinished record and finishes.
    assert!(install_to(&fresh, &fx.dest));
    assert_eq!(fx.record().phase, Phase::Committed);
    assert_eq!(fx.previous(), old);
}

/// A reader looping on the destination and on the record during repeated
/// installs never sees a missing or partial file: both are published by
/// rename.
#[test]
fn readers_never_see_a_missing_or_partial_binary_or_record() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let a = vec![b'a'; 512 * 1024];
    let b = vec![b'b'; 512 * 1024];
    let fx = Fixture::new(Some(&a));
    let src_a = fx.candidate("a", &a);
    let src_b = fx.candidate("b", &b);
    // One install first, so the record exists before the reader starts.
    assert!(install_to(&src_b, &fx.dest));

    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let (stop, dest, a, b) = (stop.clone(), fx.dest.clone(), a.clone(), b.clone());
        std::thread::spawn(move || {
            let mut reads = 0u32;
            while !stop.load(Ordering::Relaxed) {
                let got = std::fs::read(&dest).expect("the destination went missing");
                assert!(got == a || got == b, "a partial binary was visible");
                match read_record(&dest) {
                    RecordRead::Known(_) => {}
                    other => panic!("a partial or missing record was visible: {other:?}"),
                }
                reads += 1;
            }
            reads
        })
    };
    for i in 0..20 {
        assert!(install_to(if i % 2 == 0 { &src_a } else { &src_b }, &fx.dest));
    }
    stop.store(true, Ordering::Relaxed);
    assert!(reader.join().unwrap() > 0);
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// Acceptance 6: the record is tolerated when absent, old or unknown-version,
/// and an install proceeds over every one of them.
#[test]
fn an_absent_garbled_or_unknown_version_record_never_blocks_an_install() {
    let cases: [(&str, Option<&str>); 5] = [
        ("absent", None),
        ("garbled", Some("{ this is not json")),
        (
            "unknown-version",
            Some(r#"{"schema_version": 2, "phase": "resumed", "x": [1]}"#),
        ),
        ("no-version", Some(r#"{"phase": "committed"}"#)),
        ("v1-unknown-phase", Some(r#"{"schema_version": 1, "phase": "quarantined"}"#)),
    ];
    for (name, body) in cases {
        let fx = Fixture::new(Some(&stub_body("1")));
        if let Some(body) = body {
            std::fs::write(record_path(&fx.dest), body).unwrap();
        }
        let read = read_record(&fx.dest);
        match name {
            "absent" => assert_eq!(read, RecordRead::Absent),
            "unknown-version" => assert_eq!(read, RecordRead::UnknownVersion(2)),
            _ => assert!(matches!(read, RecordRead::Unreadable(_)), "{name}: {read:?}"),
        }

        assert!(install_to(&fx.candidate("fresh", &stub_body("2")), &fx.dest), "{name}");

        assert_eq!(fx.record().phase, Phase::Committed, "{name}");
        assert_eq!(fx.previous(), stub_body("1"), "{name}");
    }
}

/// Fields this binary does not know are ignored, and the two optional ones
/// may be missing: a record written by a later or an earlier build still
/// reads.
#[test]
fn a_record_with_unknown_or_missing_optional_fields_still_reads() {
    let fx = Fixture::new(None);
    let body = r#"{
        "schema_version": 1,
        "phase": "published",
        "dest": "/opt/bin/loom-daemon",
        "target": {"version": "loom-daemon 9.9.9", "sha256": "ab", "signed_by": "x"},
        "started_at": "2026-10-08T00:00:00Z",
        "updated_at": "2026-10-08T00:00:01Z",
        "roll_attempt": {"state": "armed"},
        "guard_pid": 4242
    }"#;
    std::fs::write(record_path(&fx.dest), body).unwrap();

    let RecordRead::Known(r) = read_record(&fx.dest) else {
        panic!("a v1 record with extra fields must read");
    };
    assert_eq!(r.phase, Phase::Published);
    assert_eq!(r.target.version, "loom-daemon 9.9.9");
    assert_eq!(r.previous, None);
    assert_eq!(r.installer_version, "");
}

/// The on-disk shape, pinned: a later slice and an operator both read it.
#[test]
fn the_record_on_disk_has_the_documented_shape() {
    let fx = Fixture::new(Some(&stub_body("1")));
    assert!(install_to(&fx.candidate("fresh", &stub_body("2")), &fx.dest));

    let text = std::fs::read_to_string(record_path(&fx.dest)).unwrap();
    assert!(text.ends_with('\n'));
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["phase"], "committed");
    assert_eq!(v["dest"], fx.dest.display().to_string());
    assert_eq!(v["target"]["sha256"], sha(&stub_body("2")));
    assert_eq!(v["previous"]["sha256"], sha(&stub_body("1")));
    assert_eq!(v["previous"]["path"], previous_path(&fx.dest).display().to_string());
    assert_eq!(v["installer_version"], env!("CARGO_PKG_VERSION"));
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "dest",
            "installer_version",
            "phase",
            "previous",
            "schema_version",
            "started_at",
            "target",
            "updated_at"
        ]
    );
}

#[test]
fn the_sidecar_paths_sit_beside_the_destination() {
    let dest = Path::new("/home/u/.local/bin/loom-daemon");
    assert_eq!(previous_path(dest), Path::new("/home/u/.local/bin/loom-daemon.previous"));
    assert_eq!(
        record_path(dest),
        Path::new("/home/u/.local/bin/loom-daemon.install-state.json")
    );
}
