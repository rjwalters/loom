#![allow(clippy::unwrap_used)]
use super::*;

/// Thresholds that make every age test deterministic: anything the liveness
/// rules do NOT protect is stale immediately.
fn immediate() -> Policy {
    Policy {
        exited_min_age: Duration::ZERO,
        orphan_max_age: Duration::ZERO,
        binding_max_idle: Duration::ZERO,
    }
}

fn session(workspace: &Path) -> PathBuf {
    let path = workspace.join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(path.join("data/opencode")).unwrap();
    path
}

fn write_record(directory: &Path, pid: u32, host: &str) {
    let record = SessionRecord {
        schema: 1,
        pid,
        host: host.to_owned(),
        created: 0,
    };
    fs::write(directory.join(SESSION_RECORD), serde_json::to_vec(&record).unwrap()).unwrap();
}

/// A pid that has certainly exited: a child of this process, reaped.
#[cfg(unix)]
fn exited_pid() -> u32 {
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[cfg(unix)]
#[test]
fn a_live_session_is_never_reaped_however_old_the_thresholds_say() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let live = session(&workspace);
    record_session(&live).unwrap();
    let report = reap_workspace(&workspace, &immediate(), false);
    assert!(report.sessions.is_empty());
    assert_eq!(report.kept, 1);
    assert!(live.is_dir());
}

#[cfg(unix)]
#[test]
fn a_session_whose_harness_exited_is_reaped_and_its_bytes_reported() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let dead = session(&workspace);
    fs::write(dead.join("data/opencode/blob"), vec![0u8; 4096]).unwrap();
    write_record(&dead, exited_pid(), &crate::watchdog::escalate::hostname());
    let report = reap_workspace(&workspace, &immediate(), false);
    assert_eq!(report.sessions, vec![dead.clone()]);
    assert!(report.bytes >= 4096);
    assert!(!dead.exists());
    // The default policy keeps the same directory: an exited session is only
    // reaped once the record-write race window has passed.
    let other = session(&workspace);
    write_record(&other, exited_pid(), &crate::watchdog::escalate::hostname());
    assert!(reap_workspace(&workspace, &Policy::default(), false)
        .sessions
        .is_empty());
    assert!(other.is_dir());
}

#[test]
fn a_recordless_or_foreign_session_is_aged_out_but_never_reaped_young() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let legacy = session(&workspace);
    let foreign = session(&workspace);
    write_record(&foreign, std::process::id(), "some-other-host");
    // Young: the default 6 h orphan window protects both.
    let kept = reap_workspace(&workspace, &Policy::default(), false);
    assert!(kept.sessions.is_empty());
    assert_eq!(kept.kept, 2);
    // Aged out: neither can be shown to be live on this host.
    let report = reap_workspace(&workspace, &immediate(), false);
    assert_eq!(report.sessions.len(), 2);
    assert!(!legacy.exists() && !foreign.exists());
}

#[test]
fn a_dry_run_reports_without_removing_anything() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let stale = session(&workspace);
    let report = reap_workspace(&workspace, &immediate(), true);
    assert_eq!(report.sessions, vec![stale.clone()]);
    assert!(stale.is_dir());
    assert!(summary(&report, true).starts_with("Would remove 1 stale"));
    assert!(summary(&report, false).starts_with("Removed 1 stale"));
}

#[test]
fn idle_binding_trees_and_stranded_staging_go_but_fresh_trees_stay() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let tree = workspace.join(BINDINGS).join("deadbeef");
    fs::create_dir_all(tree.join("opencode")).unwrap();
    mark_used(&tree);

    // A tree used within the idle window is kept even while sessions are reaped.
    let kept = reap_workspace(
        &workspace,
        &Policy {
            binding_max_idle: Duration::from_secs(3600),
            ..immediate()
        },
        false,
    );
    assert!(kept.bindings.is_empty());
    assert!(tree.is_dir());

    // A staging tree lost its creator by construction: publication is a rename.
    let staged = workspace
        .join(STAGING)
        .join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&staged).unwrap();
    let report = reap_workspace(&workspace, &immediate(), false);
    assert!(report.bindings.contains(&tree));
    assert!(report.bindings.contains(&staged));
    assert!(!tree.exists() && !staged.exists());
}

#[test]
fn unrecognized_entries_are_left_alone_and_a_missing_base_is_not_an_error() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    fs::create_dir_all(workspace.join("not-a-session")).unwrap();
    fs::write(workspace.join("stray-file"), b"x").unwrap();
    let report = reap_workspace(&workspace, &immediate(), false);
    assert!(report.is_empty());
    assert!(workspace.join("not-a-session").is_dir());
    assert!(workspace.join("stray-file").is_file());

    let missing = reap_base(&temp.path().join("nothing-here"), &immediate(), false);
    assert!(missing.is_empty());
    assert!(missing.errors.is_empty());
}

#[test]
fn reaping_a_base_covers_every_workspace_under_it() {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().join("native-tools");
    let mut stale = Vec::new();
    for name in ["aaaa", "bbbb"] {
        let workspace = base.join(name);
        fs::create_dir_all(&workspace).unwrap();
        stale.push(session(&workspace));
    }
    let report = reap_base(&base, &immediate(), false);
    assert_eq!(report.sessions.len(), 2);
    assert!(stale.iter().all(|path| !path.exists()));
}
