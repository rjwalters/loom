//! The machine-level install, end to end (#10983): the REAL
//! `scripts/install/provision-daemon.sh` driving the REAL
//! `loom-daemon install-binary`, in a temp dir.
//!
//! The script's retained shell suite (`tests/install/test-provision-daemon.sh`)
//! runs where no daemon is built, so it pins a shell stand-in for the helper
//! and covers the script's own half. The helper's half is unit-tested in
//! `daemon_update/provision/txn_tests.rs`. This file is the pair together:
//! what a fleet host actually runs when it rolls.
//!
//! Nothing here touches a real daemon destination, supervisor or state dir:
//! `HOME`, `TMPDIR` and `LOOM_DAEMON_BIN_DIR` all point into the test's own
//! tempdir, and the environment is cleared so no host signing identity or
//! config tier is read.

#![cfg(unix)]

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use sha2::{Digest, Sha256};

const DAEMON: &str = env!("CARGO_BIN_EXE_loom-daemon");

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/install/provision-daemon.sh")
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

struct Host {
    dir: tempfile::TempDir,
}

impl Host {
    fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("loom-install-e2e-")
            .tempdir()
            .unwrap();
        for sub in ["home", "tmp", "bin", "src"] {
            std::fs::create_dir(dir.path().join(sub)).unwrap();
        }
        Self { dir }
    }

    fn bin_dir(&self) -> PathBuf {
        self.dir.path().join("bin")
    }

    fn dest(&self) -> PathBuf {
        self.bin_dir().join("loom-daemon")
    }

    fn previous(&self) -> PathBuf {
        self.bin_dir().join("loom-daemon.previous")
    }

    fn record(&self) -> serde_json::Value {
        let text =
            std::fs::read_to_string(self.bin_dir().join("loom-daemon.install-state.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    /// A bash stand-in for a daemon build that answers `--version`.
    fn fixture(&self, name: &str, version: &str) -> (PathBuf, Vec<u8>) {
        let body = format!(
            "#!/usr/bin/env bash\nif [[ \"${{1:-}}\" == \"--version\" ]]; then echo \"loom-daemon {version}\"; fi\n"
        )
        .into_bytes();
        let path = self.dir.path().join("src").join(name);
        std::fs::write(&path, &body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        (path, body)
    }

    /// A command with a cleared environment that can only see this host.
    fn command(&self, program: &str) -> Command {
        let mut cmd = Command::new(program);
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.dir.path().join("home"))
            .env("TMPDIR", self.dir.path().join("tmp"))
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .current_dir(self.dir.path());
        cmd
    }

    /// `provision_machine_daemon <src>` from the real script. `helper` is
    /// `LOOM_DAEMON_INSTALL_HELPER`; `allow_script` is the suite-only bypass
    /// that lets a bash fixture stand in for a compiled daemon.
    fn provision(&self, src: &Path, helper: Option<&str>, allow_script: bool) -> Output {
        let mut cmd = self.command("bash");
        cmd.arg("-c")
            .arg(r#"source "$1" || exit 97; provision_machine_daemon "$2""#)
            .arg("provision")
            .arg(script())
            .arg(src)
            .env("LOOM_DAEMON_BIN_DIR", self.bin_dir());
        if let Some(helper) = helper {
            cmd.env("LOOM_DAEMON_INSTALL_HELPER", helper);
        }
        if allow_script {
            cmd.env("LOOM_PROVISION_ALLOW_SCRIPT", "1");
        }
        cmd.output().unwrap()
    }

    fn version(&self, bin: &Path) -> String {
        let out = self
            .command(bin.to_str().unwrap())
            .arg("--version")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Staging files left beside the destination.
    fn staged_residue(&self) -> Vec<String> {
        std::fs::read_dir(self.bin_dir())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".loom-install."))
            .collect()
    }
}

fn ino(p: &Path) -> u64 {
    std::fs::symlink_metadata(p).unwrap().ino()
}

fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// The headline: a roll through the machine-level path leaves the binary it
/// replaced at `loom-daemon.previous`, byte-identical, with its sha256 and
/// version recorded; a first-ever install keeps nothing.
#[test]
fn a_machine_level_install_keeps_the_previous_daemon() {
    let host = Host::new();
    let (old_src, old_body) = host.fixture("old", "1.0.0");
    let (new_src, new_body) = host.fixture("new", "2.0.0");

    let first = host.provision(&old_src, Some(DAEMON), true);
    assert!(first.status.success(), "{}", said(&first));
    assert!(!host.previous().exists(), "a first-ever install has nothing to keep");
    assert_eq!(host.record()["phase"], "committed");
    assert!(host.record()["previous"].is_null());

    let second = host.provision(&new_src, Some(DAEMON), true);
    assert!(second.status.success(), "{}", said(&second));

    assert_eq!(host.version(&host.dest()), "loom-daemon 2.0.0");
    assert_eq!(std::fs::read(host.previous()).unwrap(), old_body);
    assert_eq!(host.version(&host.previous()), "loom-daemon 1.0.0", "the copy must run");
    let record = host.record();
    assert_eq!(record["schema_version"], 1);
    assert_eq!(record["phase"], "committed");
    assert_eq!(record["previous"]["sha256"], sha(&old_body));
    assert_eq!(record["previous"]["version"], "loom-daemon 1.0.0");
    assert_eq!(record["previous"]["path"], host.previous().display().to_string());
    assert_eq!(record["target"]["version"], "loom-daemon 2.0.0");
    // The target is identified AS PUBLISHED: whatever the script's signing
    // step did to the staged file is already in these bytes.
    assert_eq!(record["target"]["sha256"], sha(&std::fs::read(host.dest()).unwrap()));
    // A bash fixture has no embedded signature, so signing leaves its bytes.
    assert_eq!(std::fs::read(host.dest()).unwrap(), new_body);
    assert_eq!(host.staged_residue(), Vec::<String>::new());
    assert!(
        std::fs::read_dir(host.dir.path().join("tmp"))
            .unwrap()
            .next()
            .is_none(),
        "the script must not leave a backup (or anything else) in TMPDIR"
    );
    assert!(said(&second).contains("installed loom-daemon"), "{}", said(&second));
}

/// The roll that first installs this change is run by an OLDER daemon, which
/// pins no helper. The script then asks the candidate itself: here a real
/// compiled daemon, passing the binary-format gate with no bypass, and (on
/// macOS) signed while still staged. What it publishes must run, and so must
/// the copy it kept of a real compiled binary.
#[test]
fn with_no_pinned_helper_the_candidate_installs_itself() {
    let host = Host::new();
    let candidate = host.dir.path().join("src").join("loom-daemon");
    std::fs::copy(DAEMON, &candidate).unwrap();
    let expected = host.version(Path::new(DAEMON));
    assert!(expected.starts_with("loom-daemon "), "{expected}");

    let first = host.provision(&candidate, None, false);
    assert!(first.status.success(), "{}", said(&first));
    assert_eq!(host.version(&host.dest()), expected);
    assert_eq!(host.record()["target"]["version"], expected);

    // Now replace that real, compiled, possibly signed binary and run the
    // copy that was kept of it.
    let live_before = std::fs::read(host.dest()).unwrap();
    let (newer, _) = host.fixture("newer", "9.9.9");
    let second = host.provision(&newer, Some(DAEMON), true);
    assert!(second.status.success(), "{}", said(&second));
    assert_eq!(host.version(&host.dest()), "loom-daemon 9.9.9");
    assert_eq!(
        sha(&std::fs::read(host.previous()).unwrap()),
        sha(&live_before),
        "the retained copy must be byte-identical to the binary that was live"
    );
    assert_eq!(
        host.version(&host.previous()),
        expected,
        "the retained copy of a real binary must still launch under this host's code signing"
    );
    assert_eq!(host.record()["previous"]["version"], expected);
}

/// When the previous binary cannot be kept, the script fails with the
/// helper's reason and the live binary is the same file it was.
#[test]
fn an_install_that_cannot_keep_the_previous_daemon_is_refused() {
    let host = Host::new();
    let (old_src, old_body) = host.fixture("old", "1.0.0");
    let (new_src, _) = host.fixture("new", "2.0.0");
    assert!(host
        .provision(&old_src, Some(DAEMON), true)
        .status
        .success());
    let before = ino(&host.dest());
    // A non-empty directory where the copy must go: it cannot be replaced.
    std::fs::create_dir(host.previous()).unwrap();
    std::fs::write(host.previous().join("in-the-way"), b"x").unwrap();

    let out = host.provision(&new_src, Some(DAEMON), true);

    assert_eq!(out.status.code(), Some(1), "{}", said(&out));
    let text = said(&out);
    assert!(text.contains("Refusing to install"), "{text}");
    assert!(text.contains("it was left untouched"), "{text}");
    assert_eq!(before, ino(&host.dest()), "the live binary must not have been replaced");
    assert_eq!(std::fs::read(host.dest()).unwrap(), old_body);
    assert_eq!(host.version(&host.dest()), "loom-daemon 1.0.0");
    assert_eq!(
        host.record()["target"]["version"],
        "loom-daemon 1.0.0",
        "the record is still the last install's"
    );
    assert_eq!(host.staged_residue(), Vec::<String>::new());
}

/// A candidate that does not load is caught while staged: the live binary is
/// not replaced, not rotated into `.previous`, and still runs.
#[test]
fn an_unloadable_candidate_never_reaches_the_live_binary() {
    let host = Host::new();
    let (old_src, old_body) = host.fixture("old", "1.0.0");
    assert!(host
        .provision(&old_src, Some(DAEMON), true)
        .status
        .success());
    let before = ino(&host.dest());
    let bad = host.dir.path().join("src").join("bad");
    std::fs::write(
        &bad,
        "#!/usr/bin/env bash\necho \"version GLIBC_2.39 not found\" >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();

    let out = host.provision(&bad, Some(DAEMON), true);

    assert_eq!(out.status.code(), Some(1), "{}", said(&out));
    let text = said(&out);
    assert!(text.contains("loadability check FAILED"), "{text}");
    assert!(text.contains("GLIBC_2.39"), "{text}");
    assert_eq!(before, ino(&host.dest()));
    assert_eq!(std::fs::read(host.dest()).unwrap(), old_body);
    assert!(!host.previous().exists(), "nothing was published, so nothing is rotated");
    assert_eq!(host.staged_residue(), Vec::<String>::new());
    let quarantined = std::fs::read_dir(host.bin_dir())
        .unwrap()
        .filter(|e| {
            let name = e.as_ref().unwrap().file_name();
            name.to_string_lossy().starts_with("loom-daemon.badglibc-")
        })
        .count();
    assert_eq!(quarantined, 1, "the bad candidate is kept as evidence");
}

/// An install killed between staging and publishing: `stage` ran, `publish`
/// never did. The live binary is untouched and runnable, and a later install
/// goes through.
#[test]
fn a_staged_install_that_is_never_published_leaves_the_live_binary_alone() {
    let host = Host::new();
    let (old_src, old_body) = host.fixture("old", "1.0.0");
    let (new_src, new_body) = host.fixture("new", "2.0.0");
    assert!(host
        .provision(&old_src, Some(DAEMON), true)
        .status
        .success());
    let before = ino(&host.dest());

    let staged = host
        .command(DAEMON)
        .args(["install-binary", "stage"])
        .arg(&new_src)
        .arg(host.dest())
        .output()
        .unwrap();

    assert!(staged.status.success(), "{}", said(&staged));
    let stdout = String::from_utf8(staged.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 1, "stage prints exactly one line: {stdout:?}");
    let staged_path = PathBuf::from(stdout.trim_end());
    assert_eq!(staged_path.parent(), Some(host.bin_dir().as_path()));
    assert_eq!(std::fs::read(&staged_path).unwrap(), new_body);
    assert_eq!(before, ino(&host.dest()));
    assert_eq!(std::fs::read(host.dest()).unwrap(), old_body);
    assert_eq!(host.version(&host.dest()), "loom-daemon 1.0.0");
    assert!(!host.previous().exists());
    assert_eq!(host.record()["target"]["version"], "loom-daemon 1.0.0");

    let later = host.provision(&new_src, Some(DAEMON), true);
    assert!(later.status.success(), "{}", said(&later));
    assert_eq!(host.version(&host.dest()), "loom-daemon 2.0.0");
    assert_eq!(std::fs::read(host.previous()).unwrap(), old_body);
}

/// `publish` renames only what `stage` made for that destination.
#[test]
fn publish_refuses_a_file_it_did_not_stage() {
    let host = Host::new();
    let (old_src, old_body) = host.fixture("old", "1.0.0");
    let (new_src, _) = host.fixture("new", "2.0.0");
    assert!(host
        .provision(&old_src, Some(DAEMON), true)
        .status
        .success());

    let out = host
        .command(DAEMON)
        .args(["install-binary", "publish"])
        .arg(&new_src)
        .arg(host.dest())
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(1), "{}", said(&out));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a staged install file"));
    assert!(out.stdout.is_empty(), "nothing human-facing on stdout");
    assert!(new_src.exists(), "a file that is not ours must not be removed");
    assert_eq!(std::fs::read(host.dest()).unwrap(), old_body);
}
