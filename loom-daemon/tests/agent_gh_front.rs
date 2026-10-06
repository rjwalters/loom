//! End-to-end: the agent `gh` front (#10331) — `loom-daemon` started as `gh`
//! — in front of a stub `next_gh` (`LOOM_GH_BIN`) that logs every call. Each
//! test runs the real binary in a child process with private cache, tmp and
//! home dirs, so no test shares env or cache state with another.

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// `api --include` reads: `304` (exit 1, as gh does) when an ETag is
/// presented, else `200` + ETag with an issue object (`…/issues/42`) or a
/// one-row listing (`…/issues?…`). Anything else echoes argv one per line,
/// stdin, the sentinel, a stderr line, and exits `$STUB_EXIT`.
const STUB: &str = r#"#!/bin/sh
printf '%s\n' "$*" >> "$STUB_LOG"
case "$*" in
  *'If-None-Match: '*)
    printf 'HTTP/2.0 304 Not Modified\r\n\r\n'
    echo 'gh: Not Modified (HTTP 304)' 1>&2
    exit 1
    ;;
  'api --include repos/o/r/issues/42')
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"v1"\r\n\r\n'
    printf '{"number": 42, "node_id": "I_1", "state": "open", "labels": [{"node_id": "LA_1", "name": "loom:issue", "description": null, "color": "fff"}]}\n'
    exit 0
    ;;
  'api --include repos/o/r/pulls/42'|'api --include repos/o/r/pulls/43')
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"p1"\r\n\r\n'
    n="${3##*/}"
    printf '{"number": %s, "head": {"ref": "feat-%s", "sha": "sha%s"}, "base": {"ref": "main"}}\n' "$n" "$n" "$n"
    exit 0
    ;;
  'api --include repos/o/r/commits/sha42/check-runs?per_page=100')
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"c1"\r\n\r\n'
    printf '{"total_count": 2, "check_runs": [{"name": "build", "status": "completed", "conclusion": "success", "started_at": "2026-10-01T00:00:00Z", "completed_at": "2026-10-01T00:00:05Z", "details_url": "https://ci/build"}, {"name": "lint", "status": "completed", "conclusion": "failure", "started_at": "2026-10-01T00:00:01Z", "completed_at": "2026-10-01T00:01:01Z", "details_url": "https://ci/lint"}]}\n'
    exit 0
    ;;
  'api --include repos/o/r/commits/sha43/check-runs?per_page=100')
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"c2"\r\n\r\n'
    printf '{"total_count": 0, "check_runs": []}\n'
    exit 0
    ;;
  'api --include repos/o/r/commits/sha4'[23]'/status?per_page=100')
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"s1"\r\n\r\n'
    printf '{"state": "pending", "total_count": 0, "statuses": []}\n'
    exit 0
    ;;
  'api --include repos/o/r/issues?'*)
    printf 'HTTP/2.0 200 OK\r\nEtag: W/"l1"\r\n\r\n'
    printf '[{"number": 7, "title": "seven", "state": "open", "labels": [{"name": "loom:issue"}], "created_at": "2026-10-01T00:00:00Z", "updated_at": "2026-10-02T00:00:00Z", "closed_at": null, "body": null, "user": {"login": "x"}}]\n'
    exit 0
    ;;
esac
for a in "$@"; do printf 'ARG:%s\n' "$a"; done
printf 'SENTINEL:%s\n' "$LOOM_GH_FRONT_ACTIVE"
printf 'STDIN:%s\n' "$(cat)"
echo 'stub stderr' 1>&2
exit "${STUB_EXIT:-0}"
"#;

/// `gh`'s shape: fields sorted, each label in `gh`'s `{id,name,description,color}` order.
const VIEW_JSON: &str = "{\"labels\":[{\"id\":\"LA_1\",\"name\":\"loom:issue\",\"description\":\
                         \"\",\"color\":\"fff\"}],\"state\":\"OPEN\"}\n";

struct Sandbox {
    root: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for d in ["bin", "work", "tmp", "home"] {
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

    fn command(&self, program: &Path, args: &[&str], env: &[(&str, &str)]) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(self.p("work"))
            .env("LOOM_GH_BIN", self.p("next-gh"))
            .env("STUB_LOG", self.p("calls.log"))
            .env("TMPDIR", self.p("tmp"))
            .env("HOME", self.p("home"))
            .env("LOOM_LISTING_CACHE_DIR", self.p("tmp/etag-cache"))
            .env("LOOM_FORGE_TYPE", "github")
            .env("GH_CACHE_OUTCOME_LOG", self.p("outcomes.log"))
            .stdin(Stdio::null());
        for k in [
            "LOOM_GH_NO_CACHE",
            "GH_CACHE_DISABLE",
            "LOOM_ETAG_LIST_DISABLE",
            "LOOM_GH_FRONT_ACTIVE",
            "LOOM_REPO",
            "GH_REPO",
            "GH_HOST",
            "STUB_EXIT",
            "GH_FORCE_TTY",
            "CLICOLOR_FORCE",
            "GH_DEBUG",
            "DEBUG",
        ] {
            cmd.env_remove(k);
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd
    }

    fn gh(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.command(&self.p("bin/gh"), args, env).output().unwrap()
    }

    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.p("calls.log"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn outcomes(&self) -> Vec<String> {
        std::fs::read_to_string(self.p("outcomes.log"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let v: serde_json::Value = serde_json::from_str(l).unwrap();
                v["x-loom-cache"].as_str().unwrap().to_string()
            })
            .collect()
    }
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

const VIEW: &[&str] = &[
    "issue",
    "view",
    "42",
    "--json",
    "labels,state",
    "--repo",
    "o/r",
];

#[test]
fn repeat_view_is_one_conditional_request_answered_304() {
    let s = Sandbox::new();
    let first = s.gh(VIEW, &[]);
    assert!(first.status.success(), "{first:?}");
    assert_eq!(stdout(&first), VIEW_JSON);
    assert_eq!(s.calls(), ["api --include repos/o/r/issues/42"]);

    // The repeat: exactly one conditional request, answered 304 (no quota),
    // served from the stored body — never a second unconditional read.
    let second = s.gh(VIEW, &[]);
    assert_eq!(stdout(&second), VIEW_JSON);
    let calls = s.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[1], "api --include repos/o/r/issues/42 -H If-None-Match: W/\"v1\"");
    assert_eq!(s.outcomes(), ["revalidated", "revalidated"]);
}

#[test]
fn list_is_etag_served_compact_with_gh_default_limit() {
    let s = Sandbox::new();
    // Fields out of order: gh sorts them, so the front must too.
    let args = [
        "issue",
        "list",
        "--label",
        "loom:issue",
        "--json",
        "title,number",
        "-R",
        "o/r",
    ];
    for _ in 0..2 {
        let out = s.gh(&args, &[]);
        assert!(out.status.success(), "{out:?}");
        assert_eq!(stdout(&out), "[{\"number\":7,\"title\":\"seven\"}]\n");
    }
    let calls = s.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert!(calls[0].starts_with("api --include repos/o/r/issues?labels=loom:issue"));
    assert!(calls[1].contains("If-None-Match: W/\"l1\""), "{calls:?}");
}

const CHECKS: &[&str] = &["pr", "checks", "42", "--repo", "o/r"];

#[test]
fn pr_checks_is_served_from_rest_and_a_repeat_is_304_only() {
    let s = Sandbox::new();
    let want = "lint\tfail\t1m0s\thttps://ci/lint\t\nbuild\tpass\t5s\thttps://ci/build\t\n";
    let first = s.gh(CHECKS, &[]);
    assert_eq!((stdout(&first).as_str(), first.status.code()), (want, Some(1)), "{first:?}");
    assert!(first.stderr.is_empty(), "{first:?}");
    assert_eq!(
        s.calls(),
        [
            "api --include repos/o/r/pulls/42",
            "api --include repos/o/r/commits/sha42/check-runs?per_page=100",
            "api --include repos/o/r/commits/sha42/status?per_page=100",
        ]
    );
    // The repeat: three conditional requests, each answered 304 (no quota).
    let second = s.gh(CHECKS, &[]);
    assert_eq!((stdout(&second).as_str(), second.status.code()), (want, Some(1)));
    let calls = s.calls();
    assert_eq!(calls.len(), 6, "{calls:?}");
    assert!(calls[3..].iter().all(|c| c.contains("If-None-Match: W/")), "{calls:?}");
    assert_eq!(s.outcomes(), ["revalidated", "revalidated"]);

    // `--json` over the same reads: compact, sorted keys, exit 0.
    let json = s.gh(&["pr", "checks", "42", "-R", "o/r", "--json", "name,bucket"], &[]);
    assert_eq!(
        (stdout(&json).as_str(), json.status.code()),
        (
            "[{\"bucket\":\"fail\",\"name\":\"lint\"},{\"bucket\":\"pass\",\"name\":\"build\"}]\n",
            Some(0)
        )
    );
}

#[test]
fn pr_checks_with_no_checks_prints_gh_empty_read_signature() {
    let s = Sandbox::new();
    for args in [
        &["pr", "checks", "43", "--repo", "o/r"][..],
        &[
            "pr",
            "checks",
            "43",
            "--repo",
            "o/r",
            "--json",
            "bucket,name",
        ][..],
    ] {
        let out = s.gh(args, &[]);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        assert_eq!(stdout(&out), "");
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            "no checks reported on the 'feat-43' branch\n"
        );
    }
}

#[test]
fn pr_checks_shapes_it_cannot_reproduce_pass_through() {
    let s = Sandbox::new();
    for extra in [&["--watch"][..], &["--required"], &["--json", "workflow"]] {
        let mut args = CHECKS.to_vec();
        args.extend_from_slice(extra);
        let out = s.gh(&args, &[]);
        assert!(stdout(&out).contains("ARG:checks"), "{extra:?}: {out:?}");
    }
    // Forced colour changes gh's output; the escape hatches force a real call.
    for env in [
        ("CLICOLOR_FORCE", "1"),
        ("LOOM_GH_NO_CACHE", "1"),
        ("GH_CACHE_DISABLE", "1"),
    ] {
        let out = s.gh(CHECKS, &[env]);
        assert!(stdout(&out).contains("ARG:checks"), "{env:?}: {out:?}");
    }
    assert!(s.calls().iter().all(|c| !c.starts_with("api ")), "{:?}", s.calls());
}

/// `pr view --json statusCheckRollup` is not served (#10516 slice B split,
/// tracked in #10629): `gh` prints the GraphQL `contexts` order, which
/// no REST read exposes, so the front cannot reproduce it exactly. It must reach
/// the real `gh` byte-identically, with no REST read in front of it, with or
/// without the escape hatches.
#[test]
fn pr_view_status_check_rollup_passes_through() {
    let s = Sandbox::new();
    let shapes: [&[&str]; 3] = [
        &[
            "pr",
            "view",
            "42",
            "--json",
            "statusCheckRollup",
            "--repo",
            "o/r",
        ],
        &[
            "pr",
            "view",
            "42",
            "--json",
            "state,statusCheckRollup",
            "-R",
            "o/r",
        ],
        &[
            "pr",
            "view",
            "42",
            "--json",
            "statusCheckRollup",
            "--repo",
            "o/r",
            "--jq",
            ".statusCheckRollup[].conclusion",
        ],
    ];
    for args in shapes {
        for env in [
            None,
            Some(("LOOM_GH_NO_CACHE", "1")),
            Some(("GH_CACHE_DISABLE", "1")),
        ] {
            let out = s.gh(args, &env.into_iter().collect::<Vec<_>>());
            let expected: String = args
                .iter()
                .map(|a| format!("ARG:{a}\n"))
                .collect::<String>()
                + "SENTINEL:1\nSTDIN:\n";
            assert_eq!(stdout(&out), expected, "{args:?} {env:?}: {out:?}");
        }
    }
    assert!(s.calls().iter().all(|c| !c.starts_with("api ")), "{:?}", s.calls());
    assert!(s.outcomes().iter().all(|o| o == "bypass"), "{:?}", s.outcomes());
}

#[test]
fn mutation_passes_through_byte_identical() {
    let s = Sandbox::new();
    let args = [
        "issue",
        "edit",
        "42",
        "--body",
        "a  b\n'c'",
        "--add-label",
        "loom:pr",
    ];
    let mut child = s
        .command(&s.p("bin/gh"), &args, &[("STUB_EXIT", "5")])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"from stdin")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(5));
    let expected: String = args
        .iter()
        .map(|a| format!("ARG:{a}\n"))
        .collect::<String>()
        + "SENTINEL:1\nSTDIN:from stdin\n";
    assert_eq!(stdout(&out), expected);
    assert_eq!(String::from_utf8_lossy(&out.stderr), "stub stderr\n");
    assert!(s.calls()[0].starts_with("issue edit 42 --body a  b"));
    assert_eq!(s.outcomes(), ["bypass"]);
}

#[test]
fn escape_hatches_and_declines_pass_through() {
    let s = Sandbox::new();
    for env in [
        ("LOOM_GH_NO_CACHE", "1"),
        ("GH_CACHE_DISABLE", "1"),
        ("LOOM_ETAG_LIST_DISABLE", "1"),
        ("GH_HOST", "ghe.example.com"),
        ("GH_REPO", "ghe.example.com/o/r"),
    ] {
        let out = s.gh(&["issue", "view", "42", "--json", "state"], &[env]);
        assert!(stdout(&out).starts_with("ARG:issue\n"), "{env:?}: {out:?}");
    }
    // Shapes the view module cannot reproduce; an implicit repo outside a checkout.
    for args in [
        &["issue", "view", "42", "--json", "author", "--repo", "o/r"][..],
        &["issue", "view", "42", "--comments", "--repo", "o/r"][..],
        &["issue", "view", "42", "--json", "state"][..],
        &["api", "repos/o/r/issues/42/comments"][..],
    ] {
        let out = s.gh(args, &[]);
        assert!(stdout(&out).starts_with(&format!("ARG:{}\n", args[0])), "{args:?}: {out:?}");
    }
    assert!(s.calls().iter().all(|c| !c.starts_with("api --include")), "{:?}", s.calls());
}

#[test]
fn implicit_repo_comes_from_the_lone_github_origin() {
    let s = Sandbox::new();
    let git = |args: &[&str]| {
        let st = Command::new("git")
            .args(args)
            .current_dir(s.p("work"))
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(st.success());
    };
    git(&["init", "-q"]);
    git(&["remote", "add", "origin", "git@github.com:o/r.git"]);
    let out = s.gh(&["issue", "view", "42", "--json", "labels,state"], &[("LOOM_REPO", "x/y")]);
    assert_eq!(stdout(&out), VIEW_JSON, "{out:?}");
    assert_eq!(s.calls(), ["api --include repos/o/r/issues/42"]);

    // GH_REPO outranks the checkout, as it does for gh.
    // (The stub has no `p/q`, so the module declines and gh answers.)
    let out = s.gh(&["issue", "view", "42", "--json", "state"], &[("GH_REPO", "p/q")]);
    assert!(stdout(&out).starts_with("ARG:issue\n"), "{out:?}");
    let calls = s.calls();
    assert_eq!(
        calls[1..],
        [
            "api --include repos/p/q/issues/42",
            "issue view 42 --json state"
        ],
        "{calls:?}"
    );
}

#[test]
fn nested_front_passes_through_and_a_loop_is_refused() {
    let s = Sandbox::new();
    let out = s.gh(VIEW, &[("LOOM_GH_FRONT_ACTIVE", "1")]);
    assert!(stdout(&out).starts_with("ARG:issue\n"), "{out:?}");
    assert!(stdout(&out).contains("SENTINEL:2\n"), "{out:?}");
    assert_eq!(s.outcomes(), Vec::<String>::new(), "a nested call is not the front's");

    let out = s.gh(VIEW, &[("LOOM_GH_FRONT_ACTIVE", "8")]);
    assert_eq!(out.status.code(), Some(127));
    assert_eq!(s.calls().len(), 1);
}

#[test]
fn loom_daemon_gh_subcommand_is_the_same_front() {
    let s = Sandbox::new();
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_loom-daemon"));
    let mut args = vec!["gh"];
    args.extend_from_slice(VIEW);
    let out = s.command(&bin, &args, &[]).output().unwrap();
    assert_eq!(stdout(&out), VIEW_JSON, "{out:?}");
    let out = s
        .command(&bin, &["gh", "pr", "merge", "7"], &[])
        .output()
        .unwrap();
    assert!(stdout(&out).starts_with("ARG:pr\nARG:merge\nARG:7\n"), "{out:?}");
}

#[test]
fn gh_shim_path_links_gh_to_this_binary() {
    let s = Sandbox::new();
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_loom-daemon"));
    let base = s.p("shim-base");
    let env = [("LOOM_GH_SHIM_BASE", base.to_str().unwrap())];
    let out = s
        .command(&bin, &["gh-shim", "path"], &env)
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let dir = PathBuf::from(stdout(&out).trim());
    assert!(dir.starts_with(&base));
    assert_eq!(std::fs::read_link(dir.join("gh")).unwrap(), bin.canonicalize().unwrap());
    // Idempotent.
    let again = s
        .command(&bin, &["gh-shim", "path"], &env)
        .output()
        .unwrap();
    assert_eq!(stdout(&again), stdout(&out));
    let bad = s.command(&bin, &["gh-shim"], &env).output().unwrap();
    assert_eq!(bad.status.code(), Some(2));
}

/// #10516: `gh-shim session-env` (the SessionStart hook) writes the front into
/// `$CLAUDE_ENV_FILE` once — with the managed launcher ahead of it under a
/// policy, as for a worker — prints nothing on stdout, always exits 0, and
/// no-ops without an env file, under `LOOM_GH_SHIM=0`, or outside a workspace.
#[test]
fn gh_shim_session_env_puts_the_front_on_the_session_path_once() {
    let s = Sandbox::new();
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_loom-daemon"));
    std::fs::create_dir_all(s.p("work/.loom")).unwrap();
    std::fs::write(s.p("work/.loom/config.json"), "{}").unwrap();
    let base = s.p("shim-base").display().to_string();
    let work = s.p("work").display().to_string();
    let run = |env_file: &Path, extra: &[(&str, &str)]| {
        let file = env_file.display().to_string();
        let mut env = vec![
            ("LOOM_GH_SHIM_BASE", base.as_str()),
            ("CLAUDE_ENV_FILE", file.as_str()),
            ("LOOM_PROJECT_ROOT", work.as_str()),
        ];
        env.extend_from_slice(extra);
        let mut cmd = s.command(&bin, &["gh-shim", "session-env"], &[]);
        for k in [
            "LOOM_GH_SHIM",
            "LOOM_FORGE_EGRESS_POLICY",
            "LOOM_FORGE_EGRESS_MANAGED",
        ] {
            cmd.env_remove(k);
        }
        let out = cmd.envs(env).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        assert!(out.stdout.is_empty(), "{out:?}");
        std::fs::read_to_string(env_file).unwrap_or_default()
    };
    let sourced_status = |env_file: &Path, extra: &[(&str, &str)]| {
        let script = format!(". '{}'; '{}' gh-shim status", env_file.display(), bin.display());
        let mut cmd = s.command(Path::new("sh"), &["-c", &script], &[]);
        cmd.env("PATH", "/usr/bin:/bin")
            .env_remove("LOOM_FORGE_EGRESS_POLICY")
            .envs(extra.iter().copied());
        stdout(&cmd.output().unwrap())
    };

    // No-ops.
    assert_eq!(run(&s.p("a.sh"), &[("CLAUDE_ENV_FILE", "")]), "");
    assert_eq!(run(&s.p("b.sh"), &[("LOOM_GH_SHIM", "0")]), "");
    let home = s.p("home").display().to_string();
    assert_eq!(run(&s.p("c.sh"), &[("LOOM_PROJECT_ROOT", home.as_str())]), "");
    assert!(!s.p("a.sh").exists() && !s.p("b.sh").exists() && !s.p("c.sh").exists());

    // Written once; sourcing it makes plain `gh` the front.
    let front = run(&s.p("d.sh"), &[]);
    assert_eq!(run(&s.p("d.sh"), &[]), front, "a second SessionStart must not append");
    assert_eq!(front.lines().count(), 1, "{front}");
    assert!(front.contains(&base), "{front}");
    let status = sourced_status(&s.p("d.sh"), &[]);
    assert!(status.starts_with("front: ") && status.contains(&base), "{status}");

    // Under a policy the managed launcher comes first, exactly as for a worker.
    let managed = s.p("managed/gh");
    std::fs::create_dir_all(managed.parent().unwrap()).unwrap();
    std::fs::write(&managed, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&managed, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut policy: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/forge-egress/policy.example.json")).unwrap();
    policy["toolchain"]["launcherPath"] = managed.display().to_string().into();
    policy["enforcement"]["api"] = "observe".into();
    let policy_file = s.p("policy.json");
    std::fs::write(&policy_file, policy.to_string()).unwrap();
    let policy_env = policy_file.display().to_string();
    let with_policy = [("LOOM_FORGE_EGRESS_POLICY", policy_env.as_str())];
    let line = run(&s.p("e.sh"), &with_policy);
    let managed_dir = s.p("managed").display().to_string();
    let (at_launcher, at_front) = (line.find(&managed_dir).unwrap(), line.find(&base).unwrap());
    assert!(at_launcher < at_front, "{line}");
    let status = sourced_status(&s.p("e.sh"), &with_policy);
    assert_eq!(status.trim(), format!("launcher: {}", managed.display()), "{status}");
}
