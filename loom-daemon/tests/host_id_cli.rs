//! `loom-daemon host-id` and the shell exporters that call it (#10023).
//!
//! Pins the cross-process half of the stable-host-id contract the in-crate
//! unit tests cannot: two separate processes resolve the same persisted id,
//! `$HOSTNAME` never leaks into it, and a shell exporter's `host_id` is
//! byte-identical to what the binary prints.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_loom-daemon")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// A command with every host-identity input pinned: no `LOOM_HOST_ID`, no
/// host-level config tier, the id file at `id_file`, and `$HOSTNAME` set to a
/// value the id must never equal.
fn isolated(mut cmd: Command, id_file: &Path, hostname: &str) -> Command {
    cmd.env_remove("LOOM_HOST_ID")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_HOST_ID_FILE", id_file)
        .env("HOSTNAME", hostname);
    cmd
}

fn host_id(id_file: &Path, hostname: &str, args: &[&str]) -> String {
    let mut cmd = isolated(Command::new(bin()), id_file, hostname);
    let out = cmd.arg("host-id").args(args).output().unwrap();
    assert!(out.status.success(), "host-id failed: {out:?}");
    String::from_utf8(out.stdout)
        .unwrap()
        .trim_end()
        .to_string()
}

#[test]
fn two_processes_resolve_one_persisted_id_that_is_never_the_hostname() {
    let dir = tempfile::tempdir().unwrap();
    let id_file = dir.path().join("dot-loom").join("host-id");

    let first = host_id(&id_file, "shell-exported", &["--source"]);
    let (id, source) = first.split_once('\t').unwrap();
    assert_eq!(source, "generated", "first use creates the id: {first}");
    assert!(id.starts_with("loom-host-"), "{id}");
    assert_ne!(id, "shell-exported");

    // A second process, from a "different launch context" ($HOSTNAME changed).
    let second = host_id(&id_file, "launchd-context", &["--source"]);
    assert_eq!(second, format!("{id}\tpersisted"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&id_file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "owner-only, got {mode:o}");
    }

    let json: serde_json::Value =
        serde_json::from_str(&host_id(&id_file, "x", &["--json"])).unwrap();
    assert_eq!(json["host_id"], id);
    assert_eq!(json["source"], "persisted");
}

#[test]
fn loom_host_id_wins_and_blank_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let id_file = dir.path().join("host-id");
    std::fs::write(&id_file, "persisted-one\n").unwrap();

    let mut cmd = isolated(Command::new(bin()), &id_file, "h");
    let out = cmd
        .env("LOOM_HOST_ID", "loom-worker-9")
        .arg("host-id")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "loom-worker-9");

    let mut cmd = isolated(Command::new(bin()), &id_file, "h");
    let out = cmd
        .env("LOOM_HOST_ID", "  ")
        .arg("host-id")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "persisted-one");
}

/// No resolvable identity is an error, not a value: the plain form prints
/// nothing on stdout and exits non-zero; `--json` still names the source.
#[test]
fn no_identity_exits_non_zero_without_printing_unknown_host() {
    let dir = tempfile::tempdir().unwrap();
    let blocker = dir.path().join("a-file");
    std::fs::write(&blocker, "").unwrap();
    let id_file = blocker.join("host-id"); // parent is a file: cannot be created

    let out = isolated(Command::new(bin()), &id_file, "h")
        .arg("host-id")
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "stdout must carry no id: {out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("LOOM_HOST_ID"), "{out:?}");

    let out = isolated(Command::new(bin()), &id_file, "h")
        .args(["host-id", "--json"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["source"], "unknown");
}

/// AC: the shell exporters report the same id as the daemon. Runs the real
/// `merge-admission-telemetry.sh record` and `lib/filing-lock.sh` against the
/// real binary (via `LOOM_DAEMON_SELF_BIN`) and compares.
#[cfg(unix)]
#[test]
fn shell_exporters_emit_the_daemons_host_id() {
    let dir = tempfile::tempdir().unwrap();
    let id_file = dir.path().join("host-id");
    let expected = host_id(&id_file, "not-this", &[]);

    let sandbox = dir.path().join("repo");
    std::fs::create_dir_all(&sandbox).unwrap();
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(&sandbox)
        .status()
        .unwrap()
        .success());
    let log = dir.path().join("admission.jsonl");
    let script = repo_root().join("defaults/scripts/merge-admission-telemetry.sh");
    let mut cmd = isolated(Command::new("bash"), &id_file, "not-this");
    let status = cmd
        .arg(&script)
        .args([
            "record",
            "--pr",
            "1",
            "--repo",
            "acme/widgets",
            "--action",
            "merge",
        ])
        .args(["--reason", "fixture"])
        .env("LOOM_DAEMON_SELF_BIN", bin())
        .env("LOOM_MERGE_ADMISSION_TELEMETRY_LOG", &log)
        .current_dir(&sandbox)
        .status()
        .unwrap();
    assert!(status.success());
    let line = std::fs::read_to_string(&log).unwrap();
    let record: serde_json::Value = serde_json::from_str(line.lines().last().unwrap()).unwrap();
    assert_eq!(record["host_id"], expected.as_str(), "{line}");

    let lib = repo_root().join("defaults/scripts/lib/filing-lock.sh");
    let mut cmd = isolated(Command::new("bash"), &id_file, "not-this");
    let out = cmd
        .arg("-c")
        .arg(r#"source "$1" && loom_filing_lock_host"#)
        .arg("bash")
        .arg(&lib)
        .env("LOOM_DAEMON_SELF_BIN", bin())
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(out.stdout).unwrap(), expected);
}
