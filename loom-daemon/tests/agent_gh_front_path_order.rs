//! End-to-end: the agent `gh` front leaves whatever `gh` came after it on
//! `PATH` reachable as its `next_gh` (#11176 item 3).
//!
//! `agent_gh::session_env` and `agent_gh::session_path` document the ordering
//! contract: the front is prepended, so the `gh` that was first before — e.g.
//! the 2am telemetry shim (`ghquota`) — becomes the front's `next_gh` and
//! keeps seeing every forge request. This pins that with no `LOOM_GH_BIN`
//! override: `PATH` is `<front dir>:<shim dir>:/usr/bin:/bin`, where the
//! shim dir holds a stub `gh` that logs each invocation, and
//!
//! - a passthrough call reaches the stub byte-identically, and
//! - a served (ETag) read reaches it too, as the conditional `gh api
//!   --include` request the front issues — so a shim second on `PATH` counts
//!   the front's in-process reads as well, including the `304`s.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

/// A stand-in for the 2am shim: logs `$*`, answers the one issue read the
/// front makes (`304` when an ETag is presented, as `gh` does), and echoes
/// argv for anything else.
const SHIM: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$SHIM_LOG"
case "$*" in
  *'If-None-Match: '*)
    printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
    echo 'gh: Not Modified (HTTP 304)' 1>&2
    exit 1
    ;;
  'api --include repos/o/r/issues/42')
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"v1"\r\n\r\n'
    printf '{"number": 42, "node_id": "I_1", "state": "open", "labels": []}\n'
    exit 0
    ;;
esac
for a in "$@"; do printf 'ARG:%s\n' "$a"; done
printf 'SENTINEL:%s\n' "$LOOM_GH_FRONT_ACTIVE"
exit "${SHIM_EXIT:-0}"
"#;

struct Host {
    root: tempfile::TempDir,
}

impl Host {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for d in ["front", "shim", "work", "tmp", "home", "sink"] {
            std::fs::create_dir_all(root.path().join(d)).unwrap();
        }
        // The front: `gh` -> loom-daemon, first on PATH.
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_loom-daemon"), root.path().join("front/gh"))
            .unwrap();
        // The shim: a real executable named `gh`, second on PATH.
        let shim = root.path().join("shim/gh");
        std::fs::write(&shim, SHIM).unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self { root }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.root.path().join(rel)
    }

    /// `gh <args>` as a session runs it: the front is the first `gh` on
    /// `PATH`, and with no `LOOM_GH_BIN` its `next_gh` comes from `PATH`
    /// alone. (Invoked by path so the lookup never depends on the test
    /// process's own `PATH`.)
    fn gh(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = std::env::join_paths([
            self.p("front"),
            self.p("shim"),
            PathBuf::from("/usr/bin"),
            PathBuf::from("/bin"),
        ])
        .unwrap();
        let mut cmd = Command::new(self.p("front/gh"));
        cmd.args(args)
            .env("PATH", &path)
            .current_dir(self.p("work"))
            .env("SHIM_LOG", self.p("shim.log"))
            .env("TMPDIR", self.p("tmp"))
            .env("HOME", self.p("home"))
            .env("LOOM_LISTING_CACHE_DIR", self.p("tmp/etag-cache"))
            .env("LOOM_FORGE_TYPE", "github")
            .env("LOOM_FORGE_CALL_STATS_DIR", self.p("sink"))
            .stdin(Stdio::null());
        for k in [
            "LOOM_GH_BIN",
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
            "LOOM_GH_BOOKED",
            "LOOM_ROLE",
            "GH_CONFIG_DIR",
            "GH_TOKEN",
            "GITHUB_TOKEN",
            "GH_CACHE_OUTCOME_LOG",
        ] {
            cmd.env_remove(k);
        }
        cmd.envs(env.iter().copied());
        cmd.output().unwrap()
    }

    fn shim_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.p("shim.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn a_passthrough_reaches_the_gh_placed_second_on_path() {
    let h = Host::new();
    let args = ["issue", "edit", "42", "--add-label", "loom:pr", "-R", "o/r"];
    let out = h.gh(&args, &[("SHIM_EXIT", "5")]);
    assert_eq!(out.status.code(), Some(5), "the shim's exit status: {out:?}");
    let expected: String = args
        .iter()
        .map(|a| format!("ARG:{a}\n"))
        .collect::<String>()
        + "SENTINEL:1\n";
    assert_eq!(stdout(&out), expected, "argv reaches the shim byte-identical");
    assert_eq!(h.shim_calls(), ["issue edit 42 --add-label loom:pr -R o/r"]);
}

#[test]
fn a_served_read_reaches_the_gh_placed_second_on_path_as_its_conditional_request() {
    let h = Host::new();
    let view = ["issue", "view", "42", "--json", "state", "--repo", "o/r"];
    for _ in 0..2 {
        let out = h.gh(&view, &[]);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(stdout(&out), "{\"state\":\"OPEN\"}\n", "served by the front");
    }
    // Both reads were served in-process, and each one's HTTP request still
    // went through the shim — the repeat as a conditional request (a 304).
    assert_eq!(
        h.shim_calls(),
        [
            "api --include repos/o/r/issues/42",
            "api --include repos/o/r/issues/42 -H If-None-Match: W/\"v1\"",
        ]
    );
}
