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
  'wait-for-signal')
    trap 'echo TERM >> "$STUB_LOG.sig"; exit 42' TERM
    : > "$STUB_LOG.ready"
    while :; do sleep 0.05; done
    ;;
  'die-by-signal')
    kill -TERM $$
    ;;
  'wait-forever')
    echo $$ > "$STUB_LOG.pid"
    : > "$STUB_LOG.ready"
    while :; do sleep 0.05; done
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
            "LOOM_FORGE_CALL_STATS_DIR",
            "LOOM_GH_BOOKED",
            "LOOM_ROLE",
            "GH_CONFIG_DIR",
            "GH_TOKEN",
            "GITHUB_TOKEN",
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

impl Sandbox {
    /// The agent rows (`ag` set) the front wrote into `sink`.
    fn agent_rows(&self) -> Vec<serde_json::Value> {
        let Ok(dir) = std::fs::read_dir(self.p("sink")) else {
            return Vec::new();
        };
        dir.flatten()
            .flat_map(|e| {
                let text = std::fs::read_to_string(e.path()).unwrap();
                text.lines()
                    .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
                    .collect::<Vec<_>>()
            })
            .filter(|v| v.get("ag").is_some())
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

/// W5: every passthrough is one forge-call ledger row, written before the
/// exec, carrying the session's role and credential — and nothing of the argv.
#[test]
fn a_passthrough_is_one_ledger_row_under_the_sessions_role_and_bucket() {
    let s = Sandbox::new();
    let sink = s.p("sink").display().to_string();
    // An owner-writer credential by path shape; no env token.
    let cred = s
        .p("ws/.loom/gh-config-by-owner/acme")
        .display()
        .to_string();
    let base = [
        ("LOOM_FORGE_CALL_STATS_DIR", sink.as_str()),
        ("GH_CONFIG_DIR", cred.as_str()),
        ("GH_TOKEN", ""),
        ("GITHUB_TOKEN", ""),
    ];
    let with = |extra: &[(&'static str, &'static str)]| {
        let mut env = base.to_vec();
        env.extend_from_slice(extra);
        env
    };
    let builder = with(&[("LOOM_ROLE", "builder")]);

    // A GraphQL-backed write and a REST read, both passed through untouched.
    let created = s.gh(&["pr", "create", "--title", "a private title", "-R", "o/r"], &builder);
    assert!(created.status.success(), "{created:?}");
    assert!(
        stdout(&created).contains("ARG:a private title"),
        "argv reaches gh byte-identical"
    );
    let files = s.gh(&["api", "repos/o/r/pulls/1/files"], &builder);
    assert!(files.status.success(), "{files:?}");
    // A different role.
    assert!(s
        .gh(&["issue", "close", "3", "-R", "o/r"], &with(&[("LOOM_ROLE", "judge")]))
        .status
        .success());
    // Not booked: a call the daemon's facade already booked, and a command
    // that never reaches the API.
    let marked = with(&[("LOOM_ROLE", "builder"), ("LOOM_GH_BOOKED", "1")]);
    assert!(s
        .gh(&["pr", "view", "1", "-R", "o/r"], &marked)
        .status
        .success());
    assert!(s.gh(&["auth", "status"], &builder).status.success());
    // Booked before the exec, so whatever the call's own outcome.
    let failed = s.gh(
        &["pr", "merge", "9", "-R", "o/r"],
        &with(&[("LOOM_ROLE", "builder"), ("STUB_EXIT", "7")]),
    );
    assert_eq!(failed.status.code(), Some(7), "the exit status is the next gh's");

    let mut rows: Vec<serde_json::Value> = Vec::new();
    for entry in std::fs::read_dir(s.p("sink")).unwrap().flatten() {
        if entry.file_name().to_string_lossy().starts_with("calls-") {
            let text = std::fs::read_to_string(entry.path()).unwrap();
            assert!(
                !text.contains("private title"),
                "an argument value reached the ledger: {text}"
            );
            rows.extend(text.lines().map(|l| serde_json::from_str(l).unwrap()));
        }
    }
    let key = |r: &serde_json::Value| {
        ["c", "p", "ir", "rp", "co", "tk"]
            .map(|k| r[k].as_str().unwrap_or("-").to_string())
            .join(" ")
    };
    let got: Vec<String> = rows.iter().map(key).collect();
    assert_eq!(
        got,
        [
            "agent.gh.pr graphql agent-builder o/r acme writer",
            "agent.gh.api core agent-builder o/r acme writer",
            "agent.gh.issue graphql agent-judge o/r acme writer",
            "agent.gh.pr graphql agent-builder o/r acme writer",
        ]
    );

    // …and they show up in the per-bucket and per-role rollups.
    let report = |by: &str| {
        let out = s
            .command(
                Path::new(env!("CARGO_BIN_EXE_loom-daemon")),
                &[
                    "forge",
                    "calls",
                    "--since",
                    "1h",
                    "--by",
                    by,
                    "--sink-dir",
                    sink.as_str(),
                ],
                &base,
            )
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        stdout(&out)
    };
    // Cells are located by their header name, never by position, so a new
    // column (INSTALLATION, #10571) cannot shift what a lookup reads.
    let table = |text: &str, first: &str| -> Vec<std::collections::BTreeMap<String, String>> {
        let mut lines = text
            .lines()
            .map(|l| l.split_whitespace().collect::<Vec<_>>());
        let header = lines
            .find(|w| w.first() == Some(&first))
            .unwrap_or_else(|| panic!("no {first} header in:\n{text}"));
        lines
            .filter(|w| w.len() == header.len())
            .map(|w| {
                header
                    .iter()
                    .map(|h| (*h).to_string())
                    .zip(w.iter().map(|c| (*c).to_string()))
                    .collect()
            })
            .collect()
    };
    let by_role = report("role");
    let role = |key: &str| {
        table(&by_role, "ROLE")
            .into_iter()
            .find(|r| r["ROLE"] == key)
            .unwrap_or_else(|| panic!("no {key} row in:\n{by_role}"))
    };
    assert_eq!(role("agent-builder")["CHARGED"], "3", "{by_role}");
    assert_eq!(role("agent-judge")["CHARGED"], "1", "{by_role}");
    let by_bucket = report("bucket");
    let bucket = |resource: &str| {
        table(&by_bucket, "ACCOUNT")
            .into_iter()
            .find(|r| r["CRED_OWNER"] == "acme" && r["RESOURCE"] == resource)
            .unwrap_or_else(|| panic!("no acme/{resource} bucket in:\n{by_bucket}"))
    };
    assert_eq!(bucket("graphql")["CHARGED"], "3", "{by_bucket}");
    assert_eq!(bucket("core")["CHARGED"], "1", "{by_bucket}");
    // The agent rows carry the installation end to end (#10571): this
    // fixture's credential dir has no identity sidecar, so it is `-`.
    assert_eq!(bucket("graphql")["INSTALLATION"], "-", "{by_bucket}");
    assert_eq!(bucket("core")["INSTALLATION"], "-", "{by_bucket}");
    assert!(report("caller").contains("agent.gh.pr"));
}

/// W5: a session's `TMPDIR` is its own `<CLAUDE_CONFIG_DIR>/tmp`, which would
/// move the default sink somewhere no host rollup reads. The spawner exports
/// the host sink as `LOOM_FORGE_CALL_STATS_DIR`; with it, the front's row
/// lands where the daemon's own `forge calls` finds it by default.
#[test]
fn a_session_row_lands_in_the_host_sink_not_under_the_session_tmpdir() {
    let s = Sandbox::new();
    // The daemon's world: its TMPDIR, and the sink that resolves from it.
    let host_tmp = s.p("host-tmp");
    let host_sink = host_tmp.join("loom-forge-call-stats");
    // The session's world, as both spawn paths set it up.
    let session_tmp = s.p("work/.loom/claude-config/builder-1/tmp");
    std::fs::create_dir_all(&host_tmp).unwrap();
    std::fs::create_dir_all(&session_tmp).unwrap();
    let (host_tmp, host_sink_s, session_tmp_s) = (
        host_tmp.display().to_string(),
        host_sink.display().to_string(),
        session_tmp.display().to_string(),
    );
    let rows_in = |dir: &Path| -> Vec<serde_json::Value> {
        let mut rows = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().starts_with("calls-") {
                let text = std::fs::read_to_string(entry.path()).unwrap();
                rows.extend(text.lines().map(|l| serde_json::from_str(l).unwrap()));
            }
        }
        rows
    };
    let session_sink = session_tmp.join("loom-forge-call-stats");

    // What the spawner exports: the host sink, and a BLANK booked marker —
    // which must not stop a real agent call from being booked.
    let exported = [
        ("TMPDIR", session_tmp_s.as_str()),
        ("LOOM_FORGE_CALL_STATS_DIR", host_sink_s.as_str()),
        ("LOOM_GH_BOOKED", ""),
        ("LOOM_ROLE", "builder"),
        ("GH_TOKEN", ""),
        ("GITHUB_TOKEN", ""),
    ];
    let out = s.gh(&["issue", "close", "1", "-R", "o/r"], &exported);
    assert!(out.status.success(), "{out:?}");
    let rows = rows_in(&host_sink);
    assert_eq!(rows.len(), 1, "the row is in the host sink: {rows:?}");
    assert_eq!(rows[0]["c"], "agent.gh.issue");
    assert_eq!(rows[0]["ir"], "agent-builder");
    assert!(rows_in(&session_sink).is_empty(), "nothing is written under the session TMPDIR");

    // The host rollup, run as the daemon runs it — the daemon's TMPDIR, no
    // override, no --sink-dir — counts it.
    let mut report = s.command(
        Path::new(env!("CARGO_BIN_EXE_loom-daemon")),
        &["forge", "calls", "--since", "1h", "--by", "role"],
        &[("TMPDIR", host_tmp.as_str())],
    );
    report.env_remove("LOOM_FORGE_CALL_STATS_DIR");
    let report = report.output().unwrap();
    assert!(report.status.success(), "{report:?}");
    let text = stdout(&report);
    let line = text
        .lines()
        .find(|l| l.split_whitespace().next() == Some("agent-builder"))
        .unwrap_or_else(|| panic!("no agent-builder row in the host rollup:\n{text}"));
    assert_eq!(line.split_whitespace().nth(1), Some("1"), "{text}");

    // The gap this closes: without the export the same call is booked under
    // the session's private tmp, where that rollup never looks.
    let mut bare = s.command(
        &s.p("bin/gh"),
        &["issue", "close", "2", "-R", "o/r"],
        &[("TMPDIR", session_tmp_s.as_str()), ("LOOM_ROLE", "builder")],
    );
    bare.env_remove("LOOM_FORGE_CALL_STATS_DIR");
    assert!(bare.output().unwrap().status.success());
    assert_eq!(rows_in(&session_sink).len(), 1, "the unexported default follows TMPDIR");
    assert_eq!(rows_in(&host_sink).len(), 1, "and the host sink did not see it");
}

/// #10607: a passthrough's one ledger row (W5) is stamped with the agent
/// role and `passthrough`, and the call keeps its streams and exit status.
#[test]
fn a_passthrough_ledger_row_carries_the_agent_role_and_via() {
    let s = Sandbox::new();
    let sink = s.p("sink").display().to_string();
    let env = [
        ("LOOM_FORGE_CALL_STATS_DIR", sink.as_str()),
        ("LOOM_ROLE", "builder"),
        ("STUB_EXIT", "3"),
    ];
    let out = s.gh(
        &[
            "pr",
            "create",
            "--title",
            "t",
            "--body",
            "secret body",
            "-R",
            "o/r",
        ],
        &env,
    );
    assert_eq!(out.status.code(), Some(3), "{out:?}");
    assert!(stdout(&out).starts_with("ARG:pr\nARG:create\n"), "{out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "stub stderr\n");
    let rows = s.agent_rows();
    assert_eq!(rows.len(), 1, "one row per passthrough: {rows:?}");
    let r = &rows[0];
    assert_eq!((&r["c"], &r["ir"]), (&"agent.gh.pr".into(), &"agent-builder".into()));
    assert_eq!((&r["ag"], &r["vi"]), (&"builder".into(), &"passthrough".into()));
    // Booked before the exec, so charged whatever the call's own exit.
    assert_eq!((&r["o"], &r["p"]), (&"ok".into(), &"graphql".into()));
    assert_eq!(
        (&r["rp"], &r["ro"], &r["ca"]),
        (&"o/r".into(), &"target".into(), &"ambient".into())
    );
    let raw = std::fs::read_dir(s.p("sink"))
        .unwrap()
        .flatten()
        .map(|e| std::fs::read_to_string(e.path()).unwrap())
        .collect::<String>();
    assert!(
        !raw.contains("secret body") && !raw.contains("--title"),
        "argv never reaches the row: {raw}"
    );
    // Every row in the sink, stamped or not: still exactly one.
    assert_eq!(raw.lines().count(), 1, "no unstamped duplicate: {raw}");
}

/// #10607: a served read keeps its facade row, now stamped `served`.
#[test]
fn a_served_issue_view_row_carries_the_agent_role_and_via() {
    let s = Sandbox::new();
    let sink = s.p("sink").display().to_string();
    let env = [
        ("LOOM_FORGE_CALL_STATS_DIR", sink.as_str()),
        ("LOOM_ROLE", "Judge"),
    ];
    for _ in 0..2 {
        assert_eq!(stdout(&s.gh(VIEW, &env)), VIEW_JSON);
    }
    let rows = s.agent_rows();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows.iter()
            .all(|r| r["c"] == "agent_gh_front" && r["ag"] == "judge" && r["vi"] == "served"),
        "{rows:?}"
    );
    assert_eq!(rows[1]["o"], "not_modified", "{rows:?}");
    // An unset role (an interactive session) is `none`, never absent.
    s.gh(&["issue", "close", "1"], &[("LOOM_FORGE_CALL_STATS_DIR", sink.as_str())]);
    assert_eq!(s.agent_rows().last().unwrap()["ag"], "none");
}

/// #10607: recording can fail; the call cannot notice.
#[test]
fn an_unwritable_sink_never_changes_the_calls_streams_or_exit_status() {
    let s = Sandbox::new();
    std::fs::write(s.p("not-a-dir"), "x").unwrap();
    let bad = s.p("not-a-dir").display().to_string();
    let args = ["issue", "edit", "42", "--add-label", "x"];
    let base = s.gh(&args, &[("STUB_EXIT", "4")]);
    let out = s.gh(
        &args,
        &[
            ("STUB_EXIT", "4"),
            ("LOOM_FORGE_CALL_STATS_DIR", bad.as_str()),
            ("LOOM_ROLE", "builder"),
        ],
    );
    assert_eq!(out.status.code(), Some(4));
    assert_eq!((out.stdout, out.stderr), (base.stdout, base.stderr));
}

/// #10607: a passthrough is an `exec`, so a signal aimed at the front is a
/// signal to `gh`, and a `gh` killed by a signal is the front killed by it.
/// Pins that contract for any future change to how passthroughs run.
#[test]
fn signals_are_forwarded_and_a_signal_death_is_mirrored() {
    use std::os::unix::process::ExitStatusExt;
    let s = Sandbox::new();
    let mut child = s
        .command(&s.p("bin/gh"), &["wait-for-signal"], &[])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let ready = s.p("calls.log.ready");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !ready.exists() {
        assert!(std::time::Instant::now() < deadline, "the stub never started");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let killed = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(42), "the stub's TERM trap ran and its code came back");
    assert_eq!(std::fs::read_to_string(s.p("calls.log.sig")).unwrap(), "TERM\n");

    let out = s.gh(&["die-by-signal"], &[]);
    assert_eq!(out.status.signal(), Some(15), "{out:?}");
}

/// #10607 review S2: `kill -9 <front>` must kill `gh` too, as it does while a
/// passthrough is an `exec` — a front that became `gh`'s parent would orphan it.
#[cfg(target_os = "linux")]
#[test]
fn a_sigkilled_front_does_not_leave_an_orphaned_gh() {
    let s = Sandbox::new();
    let mut front = s
        .command(&s.p("bin/gh"), &["wait-forever"], &[])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let ready = s.p("calls.log.ready");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !ready.exists() {
        assert!(std::time::Instant::now() < deadline, "the stub never started");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let pid = std::fs::read_to_string(s.p("calls.log.pid"))
        .unwrap()
        .trim()
        .to_string();
    // Gone = no /proc entry, or a zombie nobody has reaped yet.
    let alive = || {
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|st| {
            !st.rsplit(')')
                .next()
                .unwrap_or("")
                .trim_start()
                .starts_with('Z')
        })
    };
    assert!(alive(), "the stub is running before the kill");
    front.kill().unwrap(); // SIGKILL
    front.wait().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while alive() {
        assert!(std::time::Instant::now() < deadline, "gh {pid} outlived its SIGKILLed front");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// #10607 review S3: every spawn path exports the host sink as
/// `LOOM_FORGE_CALL_STATS_DIR` (W5), so a Builder's `cargo test` would pass it
/// to every `loom-daemon` test binary, and their fake-`gh` rows would land in
/// the real sink. Cargo resets it for whatever it runs in this repository.
#[test]
fn cargo_never_hands_a_test_binary_the_inherited_forge_call_sink() {
    let config = include_str!("../../.cargo/config.toml");
    let reset = r#"LOOM_FORGE_CALL_STATS_DIR = { value = "", force = true }"#;
    assert!(
        config.lines().any(|l| l.trim() == reset),
        ".cargo/config.toml [env] must force-reset the sink variable"
    );
    assert_eq!(
        std::env::var("LOOM_FORGE_CALL_STATS_DIR").unwrap_or_default(),
        "",
        "a test process inherited a forge-call sink"
    );
}

/// #10607 slice B: the daemon's ingest of those rows. A passthrough
/// `gh pr create` from a host session (rows in the sink itself) and a served
/// `gh issue view` from a container (whose front sees the sink's path but
/// writes the host's `contained/` subdirectory) both come out as
/// `loom.forge.calls` series carrying the agent role and served/passthrough.
#[test]
fn the_daemon_ingests_a_passthrough_pr_create_and_a_served_issue_view() {
    use loom_daemon::forge_call_stats::ingest::{drain, Cursors, Source};
    use loom_daemon::observability::ops::forge_calls::CallOutcome;
    let s = Sandbox::new();
    let sink = s.p("sink");
    let contained = sink.join("contained");
    let now = || chrono::Utc::now().timestamp();
    let mut cursors = Cursors::default();
    assert!(drain(&sink, &mut cursors, now()).calls.is_empty(), "priming");

    let host = sink.display().to_string();
    let create = ["pr", "create", "--title", "t", "--body", "b", "-R", "o/r"];
    let env = [
        ("LOOM_FORGE_CALL_STATS_DIR", host.as_str()),
        ("LOOM_ROLE", "builder"),
    ];
    assert!(s.gh(&create, &env).status.success());
    let boxed = contained.display().to_string();
    let env = [
        ("LOOM_FORGE_CALL_STATS_DIR", boxed.as_str()),
        ("LOOM_ROLE", "judge"),
    ];
    for _ in 0..2 {
        assert_eq!(stdout(&s.gh(VIEW, &env)), VIEW_JSON);
    }

    let got = drain(&sink, &mut cursors, now());
    assert_eq!(got.rejected, 0, "{got:?}");
    let calls = got.calls;
    assert_eq!(calls.len(), 3, "{calls:?}");
    let pr = calls
        .iter()
        .find(|c| c.labels.caller == "agent.gh.pr")
        .unwrap();
    assert_eq!((pr.agent, pr.via, pr.source), ("builder", "passthrough", Source::Host));
    assert_eq!((pr.labels.outcome, pr.labels.resource.as_str()), (CallOutcome::Ok, "graphql"));
    assert_eq!(pr.labels.target_owner, "o");
    let views: Vec<_> = calls
        .iter()
        .filter(|c| c.labels.caller == "agent_gh_front")
        .collect();
    assert!(
        views
            .iter()
            .all(|c| (c.agent, c.via, c.source) == ("judge", "served", Source::Contained)),
        "{views:?}"
    );
    let outcomes: Vec<_> = views.iter().map(|c| c.labels.outcome).collect();
    assert_eq!(outcomes, [CallOutcome::Ok, CallOutcome::NotModified]);
    assert!(drain(&sink, &mut cursors, now()).calls.is_empty(), "each row once");
}
