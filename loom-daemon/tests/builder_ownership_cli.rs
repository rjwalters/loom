//! Fresh Builder admission through the native CLI and real lease publisher.
//! Only forge transport and renewal are fake; no live forge or detached work.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

struct Fixture(tempfile::TempDir);

impl Fixture {
    fn new() -> Self {
        let f = Self(tempfile::tempdir().unwrap());
        let root = f.root();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join(".loom/scripts")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(root.join("comments"), "").unwrap();
        let publisher = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("defaults/scripts/sweep-lease-publish.sh");
        executable(
            &root.join(".loom/scripts/sweep-lease-publish.sh"),
            &format!(
                "#!/bin/sh\necho attempt >> attempts\nexec '{}' \"$@\"\n",
                publisher.display()
            ),
        );
        executable(
            &root.join(".loom/scripts/sweep-lease-renew.sh"),
            "#!/bin/sh\necho renewed > renewed\necho 424242\n",
        );
        executable(
            &root.join("bin/gh"),
            r#"#!/bin/sh
echo "$*" >> gh-calls
case " $* " in *' --include '*) printf 'HTTP/2.0 200 OK\r\n\r\n';; esac
case "$*" in
  *--method\ POST*) [ ! -f write-failed ] || exit 1; cat > posted; echo '{"id":1}';;
  *comments*) [ ! -f read-failed ] || exit 1; cat comments;;
  'repo view '*) echo acme/widget;;
  *'/pulls?'*) echo '[]';;
  'issue view '*) echo '{"state":"OPEN","url":"https://github.com/acme/widget/issues/42","labels":[{"name":"loom:building"}]}' ;;
  'pr list '*) echo '[]';;
  *) echo "unexpected fake gh: $*" >&2; exit 97;;
esac
"#,
        );
        executable(
            &root.join("bin/trust"),
            r#"#!/bin/sh
case "$*" in
  'forge trusted-comments') [ ! -f trust-failed ] || exit 2; exec jq -c '.';;
  'forge may-write'*) echo acme/widget;;
  *) exit 97;;
esac
"#,
        );
        f
    }

    fn root(&self) -> &Path {
        self.0.path()
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
        cmd.env_clear()
            .current_dir(self.root())
            .env(
                "PATH",
                format!("{}:{}", self.root().join("bin").display(), std::env::var("PATH").unwrap()),
            )
            .env("HOME", self.root())
            .env("LOOM_GH_BIN", self.root().join("bin/gh"))
            .env("LOOM_DAEMON_BIN", self.root().join("bin/trust"))
            .env("LOOM_REPO", "acme/widget")
            .env("LOOM_REPO_FACTS", "0")
            .env("LOOM_HOST_ID", "fixture-host")
            .env("LOOM_LEASE_PUBLISH_HOSTNAME", "1")
            .env("LOOM_AGENT_SESSION_PID", std::process::id().to_string());
        cmd
    }

    fn ensure(&self) -> Output {
        self.command()
            .args([
                "lease",
                "ensure",
                "42",
                "--require-ownership",
                "--watch-pid",
                &std::process::id().to_string(),
                "--max-age",
                "1",
            ])
            .output()
            .unwrap()
    }

    fn lease(&self, sweep: &str) {
        fs::write(
            self.root().join("comments"),
            serde_json::json!({
            "updated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            "body": format!("<!-- loom:lease host=fixture-host sweep={sweep} -->"),
                "user": {"login":"fixture-owner", "type":"User"},
                "author_association":"OWNER"
            })
            .to_string(),
        )
        .unwrap();
    }

    fn assert_refused(&self, out: Output, code: i32, token: &str) {
        assert_eq!(out.status.code(), Some(code), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), token);
        assert!(!self.root().join("posted").exists());
        assert!(!self.root().join("renewed").exists());
        let log =
            fs::read_to_string(self.root().join(".loom/logs/lease-ensure/issue-42.log")).unwrap();
        assert!(!log.contains("deferred publisher"), "{log}");
        assert_eq!(fs::read_to_string(self.root().join("attempts")).unwrap(), "attempt\n");
    }
}

#[test]
fn strict_cli_does_not_adopt_a_different_same_host_session() {
    let f = Fixture::new();
    f.lease("peer-session");
    let before = fs::read(f.root().join("comments")).unwrap();
    f.assert_refused(f.ensure(), 4, "PEER_OWNED");
    assert_eq!(fs::read(f.root().join("comments")).unwrap(), before);
}

#[test]
fn strict_cli_indeterminate_ownership_never_defers() {
    for failure in ["read-failed", "trust-failed", "write-failed"] {
        let f = Fixture::new();
        fs::write(f.root().join(failure), "").unwrap();
        f.assert_refused(f.ensure(), 2, "INDETERMINATE");
    }
}

#[test]
fn strict_cli_acquires_an_uncontested_attended_claim() {
    let f = Fixture::new();
    let out = f.ensure();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "OWNED");
    assert!(f.root().join("posted").is_file());
    assert!(f.root().join("renewed").is_file());
}

#[test]
fn strict_cli_reattaches_only_the_explicit_preflight_identity() {
    let f = Fixture::new();
    f.lease("proven-preflight");
    let out = f
        .command()
        .env("LOOM_SWEEP_RUN_ID", "proven-preflight")
        .args([
            "lease",
            "ensure",
            "42",
            "--require-ownership",
            "--watch-pid",
            &std::process::id().to_string(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "OWNED");
    assert!(!f.root().join("posted").exists(), "reattach must not post a duplicate");
    assert!(f.root().join("renewed").is_file());
}

#[test]
fn attended_check_cannot_hide_a_peer_with_an_inherited_sweep_string() {
    let f = Fixture::new();
    f.lease("peer-session");
    let out = f
        .command()
        .env("LOOM_SWEEP_ID", "peer-session")
        .args(["forge", "check-claim", "42", "--attended-claim"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("LEASE_ALREADY_HELD"),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
