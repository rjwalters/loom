//! End-to-end (#12207): which credential the agent `gh` front gives each
//! call. In a session the daemon pointed at its published gateway profiles
//! (`LOOM_GH_GATEWAY_DIR`) a read goes to the owner's gateway profile, and
//! with no gateway to a reader App from the daemon's root
//! (`LOOM_GH_READER_ROOT`); a write keeps the session's credential (the
//! writer) either way. Classified passthrough reads take the same order
//! (#12245), and a reader's fault reruns on the writer. The stub `next_gh` logs the `GH_CONFIG_DIR` it ran
//! under beside its argv, so the test reads off where each call went.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// `issue 42` answers `200` with an ETag; everything else succeeds quietly,
/// except under `$STUB_FAIL_DIR`, where every call fails with
/// `$STUB_FAIL_MSG` on stderr (default an edge `530`).
/// Each call logs `<GH_CONFIG_DIR>|<argv>`.
const STUB: &str = r#"#!/bin/sh
printf '%s|%s\n' "$GH_CONFIG_DIR" "$*" >> "$STUB_LOG"
if [ -n "$STUB_FAIL_DIR" ] && [ "$GH_CONFIG_DIR" = "$STUB_FAIL_DIR" ]; then
  echo "${STUB_FAIL_MSG:-HTTP 530}" >&2
  exit 1
fi
case "$*" in
  'api --include repos/acme/widget/issues/42')
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"v1"\r\n\r\n'
    printf '{"number": 42, "node_id": "I_1", "state": "open", "labels": []}\n'
    exit 0
    ;;
esac
echo ok
exit 0
"#;

const VIEW: &[&str] = &[
    "issue",
    "view",
    "42",
    "-R",
    "acme/widget",
    "--json",
    "state",
];
const COMMENT: &[&str] = &[
    "issue",
    "comment",
    "42",
    "-R",
    "acme/widget",
    "--body",
    "hi",
];
const DIFF: &[&str] = &["pr", "diff", "42", "-R", "acme/widget"];

fn private_dir(p: &Path) {
    std::fs::create_dir_all(p).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn private_file(p: &Path, body: &str) {
    std::fs::write(p, body).unwrap();
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600)).unwrap();
}

struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for d in ["bin", "work", "tmp", "home", "writer", "ws/.loom"] {
            std::fs::create_dir_all(root.path().join(d)).unwrap();
        }
        let stub = root.path().join("next-gh");
        std::fs::write(&stub, STUB).unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(
            env!("CARGO_BIN_EXE_loom-daemon"),
            root.path().join("bin").join("gh"),
        )
        .unwrap();
        Self { root }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    /// What the daemon publishes for `owner`: the base (marker naming this
    /// sandbox's `gh`) and one profile. Returns `(base, profile)`.
    fn publish_gateway(&self, owner: &str) -> (PathBuf, PathBuf) {
        let base = self.p("ws/.loom/gh-config-gateway");
        private_dir(&base);
        let gh = std::fs::canonicalize(self.p("next-gh")).unwrap();
        private_file(&base.join("gh-ok"), &gh.to_string_lossy());
        let profile = base.join(owner);
        private_dir(&profile);
        private_file(
            &profile.join("hosts.yml"),
            "github.com:\n    oauth_token: gp_test\n    api_host: gw.example.com\n",
        );
        (base, profile)
    }

    /// One reader App (`7`) in the roster, with a fresh token for `owner`.
    fn publish_reader(&self, owner: &str) -> PathBuf {
        std::fs::write(
            self.p("ws/.loom/config.json"),
            r#"{"forge": {"identities": {"readers": [{"appId": "7", "privateKeyPath": "/k/7.pem"}]}}}"#,
        )
        .unwrap();
        let dir = self.p(&format!("ws/.loom/gh-config-by-owner/{owner}/7"));
        private_dir(&dir);
        private_file(&dir.join("hosts.yml"), "github.com:\n    oauth_token: ghs_test\n");
        private_file(
            &dir.join("identity.json"),
            r#"{"appId": "7", "installationId": "70", "role": "reader", "expiresAt": "2099-01-01T00:00:00Z"}"#,
        );
        dir
    }

    fn gh(&self, args: &[&str], env: &[(&str, &Path)]) -> Output {
        self.run(&self.p("bin/gh"), args, env)
    }

    /// `loom-daemon <args>` run the way an agent session runs a subcommand
    /// (#12389), with the session's environment.
    fn cli(&self, args: &[&str], env: &[(&str, &Path)]) -> Output {
        self.run(Path::new(env!("CARGO_BIN_EXE_loom-daemon")), args, env)
    }

    fn run(&self, program: &Path, args: &[&str], env: &[(&str, &Path)]) -> Output {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(self.p("work"))
            .env("LOOM_GH_BIN", self.p("next-gh"))
            .env("STUB_LOG", self.p("calls.log"))
            .env("TMPDIR", self.p("tmp"))
            .env("HOME", self.p("home"))
            .env("LOOM_LISTING_CACHE_DIR", self.p("tmp/etag-cache"))
            .env("LOOM_FORGE_TYPE", "github")
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            // The session's own credential: the writer.
            .env("GH_CONFIG_DIR", self.p("writer"))
            .stdin(Stdio::null());
        for k in [
            "LOOM_GH_NO_CACHE",
            "GH_CACHE_DISABLE",
            "LOOM_ETAG_LIST_DISABLE",
            "LOOM_GH_FRONT_ACTIVE",
            "LOOM_REPO",
            "GH_REPO",
            "GH_HOST",
            "GH_FORCE_TTY",
            "CLICOLOR_FORCE",
            "GH_DEBUG",
            "DEBUG",
            "LOOM_FORGE_CALL_STATS_DIR",
            "LOOM_GH_BOOKED",
            "LOOM_ROLE",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "LOOM_PICK_JOURNAL",
            "LOOM_GH_GATEWAY_DIR",
            "LOOM_GH_READER_ROOT",
            "STUB_FAIL_DIR",
            "STUB_FAIL_MSG",
        ] {
            cmd.env_remove(k);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    /// `GH_CONFIG_DIR` of each logged call whose argv contains `part`.
    fn dirs_with(&self, part: &str) -> Vec<PathBuf> {
        std::fs::read_to_string(self.p("calls.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('|'))
            .filter(|(_, argv)| argv.contains(part))
            .map(|(dir, _)| PathBuf::from(dir))
            .collect()
    }

    /// `GH_CONFIG_DIR` of each logged call whose argv starts with `prefix`.
    fn dirs_for(&self, prefix: &str) -> Vec<PathBuf> {
        std::fs::read_to_string(self.p("calls.log"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('|'))
            .filter(|(_, argv)| argv.starts_with(prefix))
            .map(|(dir, _)| PathBuf::from(dir))
            .collect()
    }
}

/// #12394: the read was shed — no reader could serve it, and the writer is
/// never used for a read.
fn shed(out: &Output) {
    assert_eq!(
        out.status.code(),
        Some(75),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("read shed"));
}

fn ok(out: &Output) {
    assert!(
        out.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn in_a_gateway_workspace_reads_go_to_the_gateway_and_writes_to_the_writer() {
    let sb = Sandbox::new();
    let (base, profile) = sb.publish_gateway("acme");
    let env = [("LOOM_GH_GATEWAY_DIR", base.as_path())];

    // A served read (ETag view): the facade's conditional GET.
    let view = sb.gh(VIEW, &env);
    ok(&view);
    assert_eq!(String::from_utf8_lossy(&view.stdout), "{\"state\":\"OPEN\"}\n");
    assert_eq!(
        sb.dirs_for("api --include repos/acme/widget/issues/42"),
        std::slice::from_ref(&profile)
    );

    // A classified passthrough read.
    ok(&sb.gh(DIFF, &env));
    assert_eq!(sb.dirs_for("pr diff"), [profile]);

    // A write keeps the session's credential.
    ok(&sb.gh(COMMENT, &env));
    assert_eq!(sb.dirs_for("issue comment"), [sb.p("writer")]);
}

#[test]
fn with_no_gateway_a_served_read_goes_to_a_reader_and_a_write_to_the_writer() {
    let sb = Sandbox::new();
    let reader = sb.publish_reader("acme");
    let ws = sb.p("ws");
    let env = [("LOOM_GH_READER_ROOT", ws.as_path())];

    ok(&sb.gh(VIEW, &env));
    assert_eq!(sb.dirs_for("api --include repos/acme/widget/issues/42"), [reader]);

    ok(&sb.gh(COMMENT, &env));
    assert_eq!(sb.dirs_for("issue comment"), [sb.p("writer")]);
}

#[test]
fn with_no_gateway_a_passthrough_read_goes_to_a_reader_and_a_write_to_the_writer() {
    let sb = Sandbox::new();
    let reader = sb.publish_reader("acme");
    let ws = sb.p("ws");
    let env = [("LOOM_GH_READER_ROOT", ws.as_path())];

    let diff = sb.gh(DIFF, &env);
    ok(&diff);
    assert_eq!(String::from_utf8_lossy(&diff.stdout), "ok\n");
    assert_eq!(sb.dirs_for("pr diff"), [reader]);

    ok(&sb.gh(COMMENT, &env));
    assert_eq!(sb.dirs_for("issue comment"), [sb.p("writer")]);
}

#[test]
fn a_gateway_for_another_owner_leaves_a_passthrough_read_to_the_reader() {
    let sb = Sandbox::new();
    let (base, _) = sb.publish_gateway("other");
    let reader = sb.publish_reader("acme");
    let ws = sb.p("ws");
    let env = [
        ("LOOM_GH_GATEWAY_DIR", base.as_path()),
        ("LOOM_GH_READER_ROOT", ws.as_path()),
    ];

    ok(&sb.gh(DIFF, &env));
    assert_eq!(sb.dirs_for("pr diff"), [reader]);
}

#[test]
fn a_reader_fault_sheds_the_read_and_never_uses_the_writer() {
    let sb = Sandbox::new();
    let reader = sb.publish_reader("acme");
    let ws = sb.p("ws");
    let env = [
        ("LOOM_GH_READER_ROOT", ws.as_path()),
        ("STUB_FAIL_DIR", reader.as_path()),
    ];

    let diff = sb.gh(DIFF, &env);
    shed(&diff);
    assert_eq!(sb.dirs_for("pr diff"), std::slice::from_ref(&reader));
    // The gateway's outage marker is never written beside a reader.
    assert!(!reader.parent().unwrap().join("outage-until").exists());
}

#[test]
fn a_readers_own_refusal_sheds_the_read_and_never_uses_the_writer() {
    for msg in [
        "HTTP 404: Not Found (https://api.github.com/repos/acme/widget/pulls/42)",
        "HTTP 403: Resource not accessible by integration",
        "HTTP 403: API rate limit exceeded for installation ID 70.",
        "HTTP 401: Bad credentials",
    ] {
        let sb = Sandbox::new();
        let reader = sb.publish_reader("acme");
        let ws = sb.p("ws");
        let env = [
            ("LOOM_GH_READER_ROOT", ws.as_path()),
            ("STUB_FAIL_DIR", reader.as_path()),
            ("STUB_FAIL_MSG", Path::new(msg)),
        ];

        let diff = sb.gh(DIFF, &env);
        shed(&diff);
        assert_eq!(sb.dirs_for("pr diff"), std::slice::from_ref(&reader), "{msg}");
    }
}

#[test]
fn a_passthrough_read_skips_a_reader_directory_others_can_write_and_sheds() {
    let sb = Sandbox::new();
    let reader = sb.publish_reader("acme");
    std::fs::set_permissions(&reader, std::fs::Permissions::from_mode(0o770)).unwrap();
    let ws = sb.p("ws");

    shed(&sb.gh(DIFF, &[("LOOM_GH_READER_ROOT", ws.as_path())]));
    assert!(sb.dirs_for("pr diff").is_empty(), "no read was sent");
}

#[test]
fn a_reader_directory_others_can_write_is_not_used() {
    let sb = Sandbox::new();
    let reader = sb.publish_reader("acme");
    std::fs::set_permissions(&reader, std::fs::Permissions::from_mode(0o770)).unwrap();
    let ws = sb.p("ws");

    ok(&sb.gh(VIEW, &[("LOOM_GH_READER_ROOT", ws.as_path())]));
    assert_eq!(sb.dirs_for("api --include repos/acme/widget/issues/42"), [sb.p("writer")]);
}

#[test]
fn without_either_variable_reads_stay_on_the_writer() {
    let sb = Sandbox::new();
    sb.publish_gateway("acme");
    sb.publish_reader("acme");

    ok(&sb.gh(VIEW, &[]));
    assert_eq!(sb.dirs_for("api --include repos/acme/widget/issues/42"), [sb.p("writer")]);
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;

/// #12389: a `loom-daemon` subcommand an agent session runs (here `forge
/// wait-checks`, the Judge's and Doctor's CI wait) reads through the gateway,
/// then the reader pool, the daemon exported to the session. Before, it was
/// neither the daemon nor the front, so every read it made went to the
/// writer.
const WAIT: &[&str] = &[
    "forge",
    "wait-checks",
    "42",
    "--repo",
    "acme/widget",
    "--timeout",
    "0",
];

#[test]
fn a_cli_subcommand_in_a_gateway_session_reads_through_the_gateway() {
    let sb = Sandbox::new();
    let (base, profile) = sb.publish_gateway("acme");
    let _ = sb.cli(WAIT, &[("LOOM_GH_GATEWAY_DIR", base.as_path())]);
    let dirs = sb.dirs_with("repos/acme/widget/pulls/42");
    assert!(!dirs.is_empty(), "the PR read was never sent");
    assert!(dirs.iter().all(|d| *d == profile), "{dirs:?}");
}

#[test]
fn a_cli_subcommand_with_no_gateway_reads_on_a_reader() {
    let sb = Sandbox::new();
    let reader = sb.publish_reader("acme");
    let ws = sb.p("ws");
    let _ = sb.cli(WAIT, &[("LOOM_GH_READER_ROOT", ws.as_path())]);
    let dirs = sb.dirs_with("repos/acme/widget/pulls/42");
    assert!(!dirs.is_empty(), "the PR read was never sent");
    assert!(dirs.iter().all(|d| *d == reader), "{dirs:?}");
}

#[test]
fn a_cli_subcommand_without_either_variable_reads_on_the_writer() {
    let sb = Sandbox::new();
    let _ = sb.cli(WAIT, &[]);
    let dirs = sb.dirs_with("repos/acme/widget/pulls/42");
    assert!(!dirs.is_empty(), "the PR read was never sent");
    assert!(dirs.iter().all(|d| *d == sb.p("writer")), "{dirs:?}");
}

/// #12394: a CLI subcommand with a read route adopted never reads on the
/// writer. With its gateway failing and no reader behind it, the read is
/// shed and the command reports no answer.
#[test]
fn a_cli_subcommand_whose_gateway_fails_sheds_instead_of_using_the_writer() {
    let sb = Sandbox::new();
    let (base, profile) = sb.publish_gateway("acme");
    let _ = sb.cli(
        WAIT,
        &[
            ("LOOM_GH_GATEWAY_DIR", base.as_path()),
            ("STUB_FAIL_DIR", profile.as_path()),
        ],
    );
    let dirs = sb.dirs_with("repos/acme/widget/pulls/42");
    assert!(!dirs.is_empty(), "the gateway was tried");
    assert!(!dirs.contains(&sb.p("writer")), "the writer was used: {dirs:?}");
}
