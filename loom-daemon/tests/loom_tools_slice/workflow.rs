//! Compare each binary's observable results, including state across retries.
use super::{base, run, DAEMON, TOOLS};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Output;

#[path = "../support/write_scope_root.rs"]
mod write_scope_root;

struct Forge(tempfile::TempDir);

impl Forge {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        write_scope_root::writable_env(root);
        fs::write(root.join("gh"), include_bytes!("gh.py")).unwrap();
        fs::set_permissions(root.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
        let forge = Self(tmp);
        forge.set("comments", "");
        forge.set("labels", "[]");
        forge.set("body", "");
        forge
    }

    fn root(&self) -> &Path {
        self.0.path()
    }
    fn set(&self, name: &str, value: &str) {
        fs::write(self.root().join(name), value).unwrap()
    }
    fn get(&self, name: &str) -> String {
        fs::read_to_string(self.root().join(name)).unwrap()
    }

    fn call(&self, bin: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = base(bin, self.root(), self.root());
        cmd.env("LOOM_GH_BIN", self.root().join("gh"))
            .env("LOOM_LISTING_CACHE_DIR", self.root().join("etag"))
            .env("LOOM_WRITE_SCOPE_CACHE_DIR", self.root().join("scope"))
            .env("LOOM_FORGE_TYPE", "github")
            .envs(env.iter().copied())
            .args(args);
        run(cmd, None)
    }
}

fn compare(scenario: impl Fn(&Forge, &str) -> Vec<Output>) {
    let tools = scenario(&Forge::new(), TOOLS);
    let daemon = scenario(&Forge::new(), DAEMON);
    assert_eq!(tools, daemon, "status, stdout or stderr differed");
}

const CURATE: &[&str] = &[
    "curation-complete",
    "--repo",
    "acme/role-probe",
    "--issue",
    "12",
    "--model",
    "test",
    "--runtime",
    "test",
    "--completion-id",
    "abc",
    "--workspace-root",
    ".",
];
const NOTIFY: &[&str] = &["notify-cleared-blockers", "--closed", "10"];
const PARK: &str = "<!-- loom:park Blocked by: #10 by=curator at=2026-01-01T00:00:00Z -->";

#[test]
fn hold_diagnostics_read_only_parity() {
    compare(|f, bin| {
        let mut outputs = Vec::new();
        let mut check = |args: &[&str], code, expected: &str| {
            let mut argv = vec!["champion-hold"];
            argv.extend(args);
            argv.extend(["--repo", "acme/role-probe"]);
            let out = f.call(bin, &argv, &[("PERMISSION", "denied")]);
            assert_eq!(out.status.code(), Some(code), "{out:?}");
            assert!(String::from_utf8_lossy(&out.stdout).contains(expected), "{out:?}");
            if code != 0 {
                assert!(out.stdout.is_empty());
            }
            outputs.push(out);
        };
        check(&["classify", "--pr", "12"], 0, "state=clear");
        f.set("unreadable", "");
        check(&["classify", "--pr", "12"], 3, "");
        check(&["ratification", "--pr", "12"], 0, "verdict=clear");
        check(&["self-release-calibration"], 0, "scanned 0 merged PRs");
        check(&["merge-failed-marker", "--pr", "12", "--head", "abc"], 0, "head=abc");
        f.set("comments", "\"<!-- champion:merge-failed head=abc -->\"");
        check(&["merge-failed-marker", "--pr", "12", "--head", "abc"], 1, "");
        assert!(!f.get("calls").contains(".permissions"));
        outputs
    });
}

#[test]
fn curation_retries_complete_stale_cleanup() {
    compare(|f, bin| {
        f.set("labels", r#"["points:2","important"]"#);
        f.set("body", "<!-- loom:points=x -->");
        let invalid = f.call(bin, CURATE, &[]);
        assert!(!invalid.status.success());
        assert!(String::from_utf8_lossy(&invalid.stderr).contains("malformed"), "{invalid:?}");
        assert_eq!(f.get("labels"), r#"["points:2","important"]"#);
        f.set("body", "<!-- loom:points=5 -->");
        f.set("fail-edit", "");
        let partial = f.call(bin, CURATE, &[]);
        assert!(!partial.status.success());
        assert!(f.get("labels").contains("points:2") && f.get("labels").contains("points:5"));
        fs::remove_file(f.root().join("fail-edit")).unwrap();
        let done = f.call(bin, CURATE, &[]);
        let retry = f.call(bin, CURATE, &[]);
        assert!(done.status.success() && retry.status.success(), "{done:?}");
        assert_eq!(f.get("labels"), r#"["important", "loom:curated", "points:5"]"#);
        vec![invalid, partial, done, retry]
    });
}

#[test]
fn notification_preview_unpark_failure_and_retry() {
    compare(|f, bin| {
        f.set("body", PARK);
        f.set("labels", r#"["loom:blocked"]"#);
        let mut preview = NOTIFY.to_vec();
        preview.push("--dry-run");
        let dry = f.call(bin, &preview, &[]);
        assert!(String::from_utf8_lossy(&dry.stdout).contains("would unpark"), "{dry:?}");
        assert_eq!(f.get("labels"), r#"["loom:blocked"]"#);
        f.set("fail-edit", "");
        let failure = f.call(bin, &["notify-cleared-blockers", "--resweep"], &[]);
        assert!(failure.status.success());
        assert!(
            String::from_utf8_lossy(&failure.stderr).contains("unpark failed"),
            "{failure:?}"
        );
        assert!(f.get("comments").is_empty());
        let ordinary = f.call(bin, NOTIFY, &[]);
        let comment = f.get("comments");
        assert!(comment.contains("loom:blocker-cleared:#10") && !comment.contains("**Unparked**"));
        let retry = f.call(bin, NOTIFY, &[]);
        assert_eq!(f.get("comments"), comment);
        fs::remove_file(f.root().join("fail-edit")).unwrap();
        let success = f.call(bin, &["notify-cleared-blockers", "--resweep"], &[]);
        assert_eq!(f.get("labels"), r#"["loom:curated"]"#);
        assert!(f.get("comments").contains("**Unparked**"));
        vec![dry, failure, ordinary, retry, success]
    });
}

#[test]
fn scope_and_mode_gate_precede_mutations() {
    for args in [CURATE, NOTIFY] {
        for permission in ["denied", "unknown"] {
            let f = Forge::new();
            f.set("labels", r#"["loom:blocked"]"#);
            f.set("body", &format!("{PARK}\n<!-- loom:points=5 -->"));
            let out = f.call(TOOLS, args, &[("PERMISSION", permission)]);
            assert!(!out.stderr.is_empty(), "{out:?}");
            assert!(f.get("comments").is_empty());
            assert_eq!(f.get("labels"), r#"["loom:blocked"]"#);
            let calls = f.get("calls");
            for write in ["\"POST\"", "\"DELETE\"", "\"comment\"", "\"edit\""] {
                assert!(!calls.contains(write), "{calls}");
            }
        }
        let f = Forge::new();
        let gitea = f.call(TOOLS, args, &[("LOOM_FORGE_TYPE", "gitea")]);
        assert_eq!(gitea.status.code(), Some(3), "{gitea:?}");
        let fleet = f.call(
            TOOLS,
            args,
            &[
                ("LOOM_MODE", "fleet"),
                ("LOOM_GH_GATEWAY_DIR", f.root().to_str().unwrap()),
            ],
        );
        assert!(String::from_utf8_lossy(&fleet.stderr).contains("scoped fleet write broker"));
        assert!(!f.root().join("calls").exists());
        f.set("body", &format!("{PARK}\n<!-- loom:points=5 -->"));
        f.set("labels", r#"["loom:blocked"]"#);
        let local = f.call(
            TOOLS,
            args,
            &[
                ("LOOM_GH_GATEWAY_DIR", "/missing/fleet"),
                ("LOOM_GH_READER_ROOT", "/missing/reader"),
            ],
        );
        assert!(local.status.success(), "{local:?}");
        assert!(String::from_utf8_lossy(&local.stderr).contains("ignoring inherited"));
        assert!(f.get("labels").contains("loom:curated"), "{local:?}");
    }
}

#[test]
fn implicit_notification_cannot_follow_a_different_gh_repo() {
    let f = Forge::new();
    f.set("body", PARK);
    f.set("labels", r#"["loom:blocked"]"#);
    let out = f.call(TOOLS, NOTIFY, &[("GH_REPO", "other/project")]);
    assert!(out.status.success() && !out.stderr.is_empty(), "{out:?}");
    assert!(f.get("comments").is_empty());
    assert_eq!(f.get("labels"), r#"["loom:blocked"]"#);
}

#[test]
fn local_override_conflicting_with_fleet_config_refuses_workflow_writes() {
    let f = Forge::new();
    fs::create_dir(f.root().join(".loom-local")).unwrap();
    f.set(".loom-local/fleet-repo.json", r#"{"mode":"fleet"}"#);
    f.set(".loom-local/local.json", r#"{"mode":"session"}"#);
    for argv in [CURATE, NOTIFY] {
        let out = f.call(TOOLS, argv, &[]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("conflicting"), "{out:?}");
        assert!(!f.root().join("calls").exists());
    }
}
