//! Forge-egress validator (#9984): the vendored fixture suite shared with
//! 2AMLogic/2am, plus the `loom-daemon forge egress` CLI contract.
//!
//! `tests/fixtures/forge-egress/scenarios.json` is DATA shared with 2am:
//! each row is a static validator input, `expected` is what this validator
//! must produce and `upstream` is what 2am's `scripts/lib/github_egress.py`
//! produced for the same inputs. A new scenario in either repo lands as a row
//! here (and there). Hermetic: temp dirs only, no network, no real `gh`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use loom_daemon::forge_egress::checks::{
    version_tuple, CanaryOutcome, GhBuild, Observed, Profile, ProfileSource, LOOM_ONLY_CODES,
};
use loom_daemon::forge_egress::policy::{expected_api_host, Origin, PolicyDoc};
use loom_daemon::forge_egress::probe::read_api_host;
use loom_daemon::forge_egress::report::{codes, Section};
use loom_daemon::forge_egress::{evaluate, Mode};
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/forge-egress")
}

fn load(name: &str) -> Value {
    serde_json::from_str(&std::fs::read_to_string(fixture_dir().join(name)).unwrap()).unwrap()
}

/// RFC 7386 JSON merge patch (`null` deletes) — the generator's semantics.
fn merge_patch(target: &Value, patch: &Value) -> Value {
    let Value::Object(p) = patch else {
        return patch.clone();
    };
    let mut out = target.as_object().cloned().unwrap_or_default();
    for (k, v) in p {
        if v.is_null() {
            out.remove(k);
        } else {
            let merged = merge_patch(out.get(k).unwrap_or(&Value::Null), v);
            out.insert(k.clone(), merged);
        }
    }
    Value::Object(out)
}

fn subst(v: &Value, root: &str) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace("{root}", root)),
        Value::Array(a) => Value::Array(a.iter().map(|x| subst(x, root)).collect()),
        Value::Object(m) => Value::Object(
            m.iter()
                .map(|(k, x)| (k.replace("{root}", root), subst(x, root)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `row[key]`, else the suite-level default.
fn eff<'a>(row: &'a Value, defaults: &'a Value, key: &str) -> &'a Value {
    row.get(key).unwrap_or(&defaults[key])
}

fn write_profile(dir: &Path, shape: &str, api_host: &str) {
    std::fs::create_dir_all(dir).unwrap();
    // A placeholder credential that matches no token shape.
    let token = "fixture-placeholder-credential";
    let hosts = match shape {
        "routed" => format!("github.com:\n    oauth_token: {token}\n    user: fixture-user\n    git_protocol: https\n    api_host: {api_host}\n"),
        "token-only" => format!("github.com:\n    oauth_token: {token}\n    user: x-access-token\n"),
        "no-api-host" => format!("github.com:\n    oauth_token: {token}\n    user: fixture-user\n    git_protocol: https\n"),
        "wrong-host" => format!("github.com:\n    oauth_token: {token}\n    api_host: some-other-proxy.invalid\n"),
        other => panic!("unknown profile shape {other}"),
    };
    std::fs::write(dir.join("hosts.yml"), hosts).unwrap();
}

fn run_row(
    row: &Value,
    defaults: &Value,
    bases: &BTreeMap<&str, Value>,
) -> (Vec<String>, BTreeMap<&'static str, Vec<String>>, i32) {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().to_str().unwrap().to_string();
    let base = &bases[eff(row, defaults, "base").as_str().unwrap()];
    let policy = subst(&merge_patch(base, eff(row, defaults, "policy_patch")), &root);
    let api_host = expected_api_host(&policy);
    for (path, shape) in subst(eff(row, defaults, "profiles"), &root)
        .as_object()
        .unwrap()
    {
        write_profile(Path::new(path), shape.as_str().unwrap(), &api_host);
    }
    let env = subst(eff(row, defaults, "env"), &root);
    let env_get = |k: &str| env.get(k).and_then(Value::as_str).map(str::to_string);
    let gh_config_dir = env_get("GH_CONFIG_DIR").map(PathBuf::from);
    let mut profiles = Vec::new();
    match &gh_config_dir {
        Some(d) => profiles.push(Profile {
            path: d.clone(),
            source: ProfileSource::Env,
            api_host: read_api_host(d, "github.com"),
        }),
        None => {
            let d = PathBuf::from(
                subst(eff(row, defaults, "default_profile"), &root)
                    .as_str()
                    .unwrap(),
            );
            profiles.push(Profile {
                api_host: read_api_host(&d, "github.com"),
                path: d,
                source: ProfileSource::Default,
            });
        }
    }
    for p in subst(eff(row, defaults, "loom_owned"), &root)
        .as_array()
        .unwrap()
    {
        let d = PathBuf::from(p.as_str().unwrap());
        if gh_config_dir.as_ref() != Some(&d) {
            profiles.push(Profile {
                api_host: read_api_host(&d, "github.com"),
                path: d,
                source: ProfileSource::LoomOwned,
            });
        }
    }
    let mut gh = defaults["gh"].as_object().unwrap().clone();
    if let Some(over) = row.get("gh").and_then(Value::as_object) {
        gh.extend(over.clone());
    }
    let launcher = policy["toolchain"]["launcherPath"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let gh_path = match gh["path"].as_str() {
        Some("launcher") => Some(PathBuf::from(&launcher)),
        Some(p) => Some(PathBuf::from(p)),
        None => None,
    };
    let version = gh["version"].as_str().and_then(version_tuple);
    let obs = Observed {
        gh_host: env_get("GH_HOST"),
        gh_repo: env_get("GH_REPO"),
        gh_config_dir,
        // The fixture's single `gh.path` is 2am's one observed `gh` — the
        // PATH `gh`, which in 2am is also what runs. Loom splits the two
        // (#9995); a fixture row describes a host where they coincide.
        path_gh: gh_path.clone(),
        gh: GhBuild {
            path: gh_path,
            version,
            raw: gh["version"]
                .as_str()
                .map(|v| format!("gh version {v} (fixture)"))
                .unwrap_or_default(),
        },
        launcher_exists: gh["launcher_exists"].as_bool().unwrap(),
        profiles,
        git_rewrites: eff(row, defaults, "git_rewrites").as_u64().unwrap() as usize,
        canary: match eff(row, defaults, "canary").as_str() {
            Some("blocked") => Some(CanaryOutcome::Blocked),
            Some("open") => Some(CanaryOutcome::Open),
            None => None,
            Some(other) => panic!("unknown canary outcome {other}"),
        },
        loom_otlp_exporter: eff(row, defaults, "loom_otlp").as_bool().unwrap(),
        ..Observed::default()
    };
    let doc = PolicyDoc {
        data: policy,
        path: tmp.path().join("policy.json"),
        origin: Origin::Machine,
        ignored: vec![],
    };
    let report = evaluate(&doc, &obs, Mode::Doctor);
    let mut sections = BTreeMap::new();
    for s in Section::ALL {
        sections.insert(
            s.as_str(),
            codes(report.section(s))
                .into_iter()
                .map(str::to_string)
                .collect(),
        );
    }
    // Nothing from a profile file ever reaches the report.
    let rendered = report.to_json().to_string();
    assert!(
        !rendered.contains("fixture-placeholder-credential"),
        "{}: credential leaked: {rendered}",
        row["id"]
    );
    (report.routing_codes(), sections, report.exit_code())
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|c| c.as_str().unwrap().to_string())
        .collect()
}

#[test]
fn vendored_fixture_suite_matches_expected_and_upstream_codes() {
    let suite = load("scenarios.json");
    let defaults = &suite["defaults"];
    let mut bases = BTreeMap::new();
    bases.insert("example", load("policy.example.json"));
    bases.insert("fixture", load("policy.fixture.json"));
    let rows = suite["rows"].as_array().unwrap();
    // All seventeen upstream scenarios (0-17) are represented.
    for n in 0..=17 {
        let prefix = format!("s{n:02}-");
        assert!(
            rows.iter()
                .any(|r| r["id"].as_str().unwrap().starts_with(&prefix)),
            "scenario {n} missing"
        );
    }
    let mut failures = Vec::new();
    for row in rows {
        let id = row["id"].as_str().unwrap();
        let (_, sections, exit) = run_row(row, defaults, &bases);
        for s in Section::ALL {
            let name = s.as_str();
            let got = &sections[name];
            let expected = strings(&row["expected"][name]);
            if *got != expected {
                failures.push(format!("{id} {name}: got {got:?}, expected {expected:?}"));
            }
            // Lockstep: every 2am code is produced, in 2am's order, and every
            // extra code is a Loom-only observation.
            let upstream = strings(&row["upstream"][name]);
            let shared: Vec<&String> = got.iter().filter(|c| upstream.contains(c)).collect();
            if shared.len() != upstream.len() || shared.iter().zip(&upstream).any(|(a, b)| *a != b)
            {
                failures
                    .push(format!("{id} {name}: upstream {upstream:?} not reproduced by {got:?}"));
            }
            for extra in got.iter().filter(|c| !upstream.contains(c)) {
                if !LOOM_ONLY_CODES.contains(&extra.as_str())
                    && row.get("loom_only_surface").is_none()
                {
                    failures
                        .push(format!("{id} {name}: {extra} is neither upstream nor Loom-only"));
                }
            }
        }
        let expected_exit = row["expected"]["exit_code"].as_i64().unwrap() as i32;
        if exit != expected_exit {
            failures.push(format!("{id}: exit {exit}, expected {expected_exit}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} fixture failure(s):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

// ---------------------------------------------------------------------------
// The CLI
// ---------------------------------------------------------------------------

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // gh's default profile (no GH_CONFIG_DIR exported), correctly routed.
        let default_profile = dir.path().join("home/.config/gh");
        write_profile(&default_profile, "routed", "github-proxy.fixture.invalid");
        // A fake effective `gh` so the result never depends on this host's gh.
        let gh = dir.path().join("gh");
        std::fs::write(&gh, "#!/bin/sh\necho 'gh version 2.102.0 (fixture)'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            // PATH's `gh` (what `toolchain.launcher-not-first` measures) is
            // the same fake, so the verdict never depends on this host's PATH.
            std::fs::create_dir_all(dir.path().join("bin")).unwrap();
            std::os::unix::fs::symlink(&gh, dir.path().join("bin/gh")).unwrap();
        }
        Self { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
        c.args(["forge", "egress"])
            .args(args)
            .current_dir(self.path())
            .env_clear();
        c.env("PATH", self.path_env())
            .env("HOME", self.path().join("home"))
            .env("LOOM_GH_BIN", self.path().join("gh"))
            // A host machine policy's launcher must never outrank the fake.
            .env("LOOM_GH_NO_POLICY_LAUNCHER", "1")
            .env("LOOM_SOCKET_PATH", self.path().join("loom-daemon.sock"));
        c
    }

    /// `PATH` with the sandbox's `bin/` first.
    fn path_env(&self) -> std::ffi::OsString {
        let mut dirs = vec![self.path().join("bin")];
        dirs.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()));
        std::env::join_paths(dirs).unwrap()
    }

    fn policy(&self, mutate: impl FnOnce(&mut Value)) -> PathBuf {
        let mut p = load("policy.fixture.json");
        p = subst(&p, self.path().to_str().unwrap());
        p["toolchain"]["launcherPath"] = self.path().join("gh").display().to_string().into();
        mutate(&mut p);
        let path = self.path().join("policy.json");
        std::fs::write(&path, p.to_string()).unwrap();
        path
    }
}

fn machine_policy_present() -> bool {
    Path::new(loom_daemon::forge_egress::policy::MACHINE_POLICY_PATH).exists()
}

#[test]
fn doctor_json_with_no_policy_is_unconfigured_and_exits_zero() {
    if machine_policy_present() {
        eprintln!("skip: this host has a machine policy");
        return;
    }
    let sb = Sandbox::new();
    let out = sb.cmd(&["doctor", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["policy"]["origin"], "unconfigured");
    assert_eq!(report["exit_code"], 0);
    let quiet = sb.cmd(&["assert", "--quiet"]).output().unwrap();
    assert_eq!(quiet.status.code(), Some(0));
    assert!(quiet.stdout.is_empty() && quiet.stderr.is_empty());
}

#[test]
fn unknown_schema_version_exits_two_never_zero() {
    let sb = Sandbox::new();
    let path = sb.policy(|p| p["schemaVersion"] = 2.into());
    for args in [&["assert", "--quiet"][..], &["doctor", "--json"]] {
        let out = sb
            .cmd(args)
            .env("LOOM_FORGE_EGRESS_POLICY", &path)
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }
    let out = sb
        .cmd(&["doctor", "--json"])
        .env("LOOM_FORGE_EGRESS_POLICY", &path)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["policy"]["origin"], "env");
    assert!(report["routing"]["findings"]
        .to_string()
        .contains("policy.schema-version"));
}

#[test]
fn explicit_missing_policy_is_incomplete_not_a_fall_through() {
    let sb = Sandbox::new();
    let out = sb
        .cmd(&["doctor", "--json"])
        .env("LOOM_FORGE_EGRESS_POLICY", sb.path().join("absent.json"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(report["routing"]["findings"]
        .to_string()
        .contains("policy.unreadable"));
}

#[test]
fn below_floor_gh_with_example_policy_exits_one() {
    let sb = Sandbox::new();
    std::fs::write(sb.path().join("gh"), "#!/bin/sh\necho 'gh version 2.97.0 (2025-01-01)'\n")
        .unwrap();
    let path = sb.policy(|_| {});
    let out = sb
        .cmd(&["doctor", "--json"])
        .env("LOOM_FORGE_EGRESS_POLICY", &path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(report["routing"]["findings"]
        .to_string()
        .contains("toolchain.below-api-host-floor"));
    assert_eq!(report["git"]["exit_code"], 2, "git.unqualified is its own section");
    assert_eq!(report["observed"]["apiHostHonoured"], false);
}

#[test]
fn no_output_carries_tokens_hosts_yml_or_the_environment() {
    let sb = Sandbox::new();
    let token = format!("ghp_{}", "C".repeat(36));
    let path = sb.policy(|p| p["principal"]["credentialRef"] = token.clone().into());
    let profile = sb.path().join("agent");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(
        profile.join("hosts.yml"),
        format!("github.com:\n    oauth_token: {token}\n    user: x\n"),
    )
    .unwrap();
    for args in [
        &["assert"][..],
        &["doctor", "--json"],
        &["doctor"],
        &["policy"],
    ] {
        let out = sb
            .cmd(args)
            .env("LOOM_FORGE_EGRESS_POLICY", &path)
            .env("GH_CONFIG_DIR", &profile)
            .env("SOME_SECRET_ENV", "env-value-must-not-appear")
            .output()
            .unwrap();
        let all = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!all.contains(&token), "{args:?} leaked the token:\n{all}");
        assert!(!all.contains("CCCCCCCCCCCC"), "{args:?} leaked a token fragment");
        assert!(!all.contains("env-value-must-not-appear"), "{args:?} dumped the environment");
        assert!(!all.contains("oauth_token"), "{args:?} dumped hosts.yml");
    }
    let out = sb
        .cmd(&["assert", "--quiet"])
        .env("LOOM_FORGE_EGRESS_POLICY", &path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "an inline secret fails the assertion");
    assert!(out.stdout.is_empty());
}

#[test]
fn spawn_worker_refuses_on_a_failing_assert_under_required() {
    let sb = Sandbox::new();
    // Unknown schemaVersion: exit 2, never observe-only ⇒ refused.
    let path = sb.policy(|p| p["schemaVersion"] = 2.into());
    let spawned = sb.path().join("spawned");
    let scripts = sb.path().join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    let runtime = scripts.join("spawn-claude.sh");
    std::fs::write(&runtime, format!("#!/bin/sh\ntouch '{}'\n", spawned.display())).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["spawn-worker", "--scripts-dir"])
        .arg(&scripts)
        .args(["--", "-p", "hello"])
        .current_dir(sb.path())
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", sb.path().join("home"))
        .env("LOOM_GH_BIN", sb.path().join("gh"))
        .env("LOOM_WORKSPACE", sb.path())
        .env("LOOM_RUNTIME", "claude")
        .env("LOOM_FORGE_EGRESS_POLICY", &path)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(stderr.contains("forge-egress"), "{stderr}");
    assert!(stderr.contains("policy.schema-version"), "{stderr}");
    assert!(!spawned.exists(), "no worker may be spawned");
}

/// #9995: once the daemon execs the policy launcher itself, the version floor
/// measures that launcher while `toolchain.launcher-not-first` still measures
/// the `gh` agents get from PATH.
#[test]
#[cfg(unix)]
fn launcher_not_first_measures_path_even_when_the_daemon_execs_the_launcher() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    // PATH's gh: an unmanaged, below-floor build that is NOT the launcher.
    let unmanaged = sb.path().join("bin/gh");
    std::fs::remove_file(&unmanaged).unwrap();
    std::fs::write(&unmanaged, "#!/bin/sh\necho 'gh version 2.97.0 (unmanaged)'\n").unwrap();
    std::fs::set_permissions(&unmanaged, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = sb.policy(|_| {});
    let out = sb
        .cmd(&["doctor", "--json"])
        // No override: the exec target can only come from the policy rung.
        .env_remove("LOOM_GH_BIN")
        .env_remove("LOOM_GH_NO_POLICY_LAUNCHER")
        .env("LOOM_FORGE_EGRESS_POLICY", &path)
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let routing = report["routing"]["findings"].to_string();
    assert!(routing.contains("toolchain.launcher-not-first"), "{report:#}");
    assert!(
        !routing.contains("toolchain.below-api-host-floor"),
        "the floor measures the exec target (the launcher), not PATH's gh: {report:#}"
    );
    let launcher = sb.path().join("gh").display().to_string();
    assert_eq!(report["observed"]["ghPath"], launcher.as_str(), "{report:#}");
    assert_eq!(report["observed"]["ghVersion"], "2.102.0");
    assert_eq!(report["observed"]["pathGhPath"], unmanaged.display().to_string().as_str());
    assert_eq!(out.status.code(), Some(1));
}

/// #9995 review: `LOOM_GH_NO_POLICY_LAUNCHER=1` — the seam every gh-stubbing
/// harness sets — makes `LOOM_GH_BIN` win over an existing policy launcher.
/// The daemon really execs the winner (`gh --version`), so the reported version
/// proves which binary ran.
#[test]
#[cfg(unix)]
fn no_policy_launcher_opt_out_makes_the_stub_win_over_the_launcher() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    let stub = sb.path().join("stub-gh");
    std::fs::write(&stub, "#!/bin/sh\necho 'gh version 2.99.0 (stub)'\n").unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    // The policy's launcherPath is the sandbox's `gh` (2.102.0), and it exists.
    let path = sb.policy(|_| {});
    let launcher = sb.path().join("gh").display().to_string();
    let observed = |opt_out: Option<&str>| {
        let mut c = sb.cmd(&["doctor", "--json"]);
        c.env("LOOM_GH_BIN", &stub)
            .env("LOOM_FORGE_EGRESS_POLICY", &path);
        match opt_out {
            Some(v) => c.env("LOOM_GH_NO_POLICY_LAUNCHER", v),
            None => c.env_remove("LOOM_GH_NO_POLICY_LAUNCHER"),
        };
        let out = c.output().unwrap();
        let report: Value = serde_json::from_slice(&out.stdout).unwrap();
        let routing = report["routing"]["findings"].to_string();
        (report["observed"].clone(), routing)
    };
    let declined = "toolchain.policy-launcher-declined";

    let (o, routing) = observed(Some("1"));
    assert_eq!(o["ghPath"], stub.display().to_string().as_str(), "{o:#}");
    assert_eq!(o["ghVersion"], "2.99.0", "the stub is what ran: {o:#}");
    assert_eq!(o["ghSource"], "env_override", "{o:#}");
    // PATH's gh is the launcher, so only the exec-target finding can see this.
    assert!(routing.contains(declined), "the declined rung is reported: {routing}");
    assert!(!routing.contains("toolchain.launcher-not-first"), "{routing}");

    // Without the opt-out (or with any value but `1`) the launcher outranks
    // LOOM_GH_BIN.
    for opt_out in [None, Some("0")] {
        let (o, routing) = observed(opt_out);
        assert!(!routing.contains(declined), "{opt_out:?}: {routing}");
        assert_eq!(o["ghPath"], launcher.as_str(), "{opt_out:?}: {o:#}");
        assert_eq!(o["ghVersion"], "2.102.0", "{opt_out:?}: {o:#}");
        assert_eq!(o["ghSource"], "policy", "{opt_out:?}: {o:#}");
    }
}

/// #9987: `container-args` never lets "no output" mean "no policy". The status
/// line is explicit, and a configured policy that cannot be honoured exits 78
/// with no status (so `spawn-claude.sh` cannot restore ambient credentials).
#[test]
fn container_args_status_is_explicit_and_a_bad_configured_policy_refuses() {
    let sb = Sandbox::new();
    let run = |policy: Option<&Path>| {
        let mut c = sb.cmd(&["container-args"]);
        c.env("GH_TOKEN", "ghp_must_not_be_printed");
        c.env_remove("LOOM_GH_NO_POLICY_LAUNCHER");
        if let Some(p) = policy {
            c.env("LOOM_FORGE_EGRESS_POLICY", p);
        }
        c.output().unwrap()
    };
    if !machine_policy_present() {
        let out = run(None);
        assert_eq!(out.status.code(), Some(0));
        // The legacy credentials, by NAME: the daemon makes the whole decision.
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert_eq!(stdout, "loom-forge-egress: unconfigured\n-e\nGH_TOKEN\n");
    }
    type Mutate = fn(&mut Value);
    let cases: [(&str, Mutate); 3] = [
        ("policy.schema", |p| {
            p["toolchain"]
                .as_object_mut()
                .unwrap()
                .remove("launcherPath");
        }),
        ("policy.schema", |p| p["toolchain"]["launcherPath"] = "".into()),
        ("policy.schema-version", |p| p["schemaVersion"] = 2.into()),
    ];
    for (code, mutate) in cases {
        let path = sb.policy(|p| {
            p["enforcement"]["api"] = "required".into();
            mutate(p);
        });
        let out = run(Some(&path));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(78), "{code}: {stderr}");
        assert!(out.stdout.is_empty(), "{code}: no status, no args");
        assert!(stderr.contains(code), "{code}: {stderr}");
    }
    let path = sb.policy(|p| p["enforcement"]["api"] = "required".into());
    let out = run(Some(&path));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.starts_with("loom-forge-egress: managed\n-v\n"), "{stdout}");
    assert!(!stdout.contains("GH_TOKEN") && !stdout.contains("ghp_"), "{stdout}");
}

/// #10446 review: `LOOM_GH_NO_POLICY_LAUNCHER=1` (the resolver's stub-`gh`
/// seam, exported by the shell test harnesses) must never relax container
/// credential admission: a required policy stays managed (no token), an
/// invalid one still refuses, and a managed marker without a policy refuses.
#[test]
fn container_args_ignores_the_no_policy_launcher_opt_out() {
    let sb = Sandbox::new();
    let run = |envs: &[(&str, &std::ffi::OsStr)]| {
        let mut c = sb.cmd(&["container-args"]);
        c.env("LOOM_GH_NO_POLICY_LAUNCHER", "1")
            .env("GH_TOKEN", "ghp_must_not_leak");
        for (k, v) in envs {
            c.env(k, v);
        }
        c.output().unwrap()
    };
    let path = sb.policy(|p| p["enforcement"]["api"] = "required".into());
    let out = run(&[("LOOM_FORGE_EGRESS_POLICY", path.as_os_str())]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.starts_with("loom-forge-egress: managed\n"), "{stdout}");
    assert!(!stdout.contains("GH_TOKEN") && !stdout.contains(".config/gh"), "{stdout}");

    let path = sb.policy(|p| {
        p["enforcement"]["api"] = "required".into();
        p["toolchain"]["launcherPath"] = "".into();
    });
    let out = run(&[("LOOM_FORGE_EGRESS_POLICY", path.as_os_str())]);
    assert_eq!(out.status.code(), Some(78));
    assert!(out.stdout.is_empty());

    if !machine_policy_present() {
        let out = run(&[("LOOM_FORGE_EGRESS_MANAGED", std::ffi::OsStr::new("1"))]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(78), "{stderr}");
        assert!(out.stdout.is_empty());
        assert!(stderr.contains("policy.unconfigured"), "{stderr}");
    }
}

/// The native half of the same regression: under the opt-out a required
/// policy's launcher still goes first on the worker PATH, and a managed
/// marker without a policy still refuses the spawn.
#[test]
#[cfg(unix)]
fn spawn_worker_ignores_the_no_policy_launcher_opt_out() {
    use std::os::unix::fs::PermissionsExt;
    let sb = Sandbox::new();
    let scripts = sb.path().join("scripts");
    std::fs::create_dir_all(&scripts).unwrap();
    let seen = sb.path().join("seen-path");
    let runtime = scripts.join("spawn-claude.sh");
    std::fs::write(&runtime, format!("#!/bin/sh\nprintf '%s' \"$PATH\" >'{}'\n", seen.display()))
        .unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();
    // Routed through the gateway with no stored token: what `required` wants.
    let profile = sb.path().join("home-required/.config/gh");
    std::fs::create_dir_all(&profile).unwrap();
    std::fs::write(
        profile.join("hosts.yml"),
        "github.com:\n    user: fixture-user\n    git_protocol: https\n    api_host: github-proxy.fixture.invalid\n",
    )
    .unwrap();
    let spawn = |envs: &[(&str, &std::ffi::OsStr)]| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
        c.args(["spawn-worker", "--scripts-dir"])
            .arg(&scripts)
            .args(["--", "-p", "hello"])
            .current_dir(sb.path())
            .env_clear()
            .env("PATH", sb.path_env())
            // No hosts.yml token: `required` publishes none (#9986).
            .env("HOME", sb.path().join("home-required"))
            .env("LOOM_WORKSPACE", sb.path())
            .env("LOOM_RUNTIME", "claude")
            .env("LOOM_GH_NO_POLICY_LAUNCHER", "1");
        for (k, v) in envs {
            c.env(k, v);
        }
        c.output().unwrap()
    };
    let path = sb.policy(|p| p["enforcement"]["api"] = "required".into());
    let out = spawn(&[("LOOM_FORGE_EGRESS_POLICY", path.as_os_str())]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let seen_path = std::fs::read_to_string(&seen).unwrap();
    let first = std::env::split_paths(&seen_path).next().unwrap();
    assert_eq!(first, sb.path(), "the launcher dir leads the worker PATH: {seen_path}");

    if !machine_policy_present() {
        std::fs::remove_file(&seen).unwrap();
        let out = spawn(&[("LOOM_FORGE_EGRESS_MANAGED", std::ffi::OsStr::new("1"))]);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(78), "{stderr}");
        assert!(stderr.contains("policy.unconfigured"), "{stderr}");
        assert!(!seen.exists(), "no worker may be spawned");
    }
}

/// #9987 scope item 1: `daemon-start` puts the policy's managed launcher
/// directory first on the PATH it bakes into the systemd unit (and launchd
/// plist), so everything the daemon runs resolves the launcher as `gh`. With
/// no policy the rendered PATH is the canonical one, unchanged.
#[test]
#[cfg(unix)]
fn daemon_start_renders_the_launcher_dir_first_on_the_unit_path() {
    let sb = Sandbox::new();
    let home = sb.path().join("home");
    let repo = sb.path().join("repo");
    std::fs::create_dir_all(repo.join(".loom")).unwrap();
    let render = |policy: Option<&Path>| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
        c.args(["daemon-start", "--print-unit"])
            .current_dir(&repo)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", &home)
            .env("TMPDIR", sb.path())
            .env("LOOM_SOCKET_PATH", sb.path().join("loom-daemon.sock"))
            .env("LOOM_DAEMON_BIN", sb.path().join("gh"));
        if let Some(p) = policy {
            c.env("LOOM_FORGE_EGRESS_POLICY", p);
        }
        let out = c.output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
        let unit = String::from_utf8_lossy(&out.stdout).into_owned();
        unit.lines()
            .find_map(|l| l.strip_prefix("Environment=PATH="))
            .unwrap_or_else(|| panic!("no Environment=PATH= line:\n{unit}"))
            .to_string()
    };
    let canonical = format!("{}/.local/bin:", home.display());
    if !machine_policy_present() {
        let path = render(None);
        assert!(path.starts_with(&canonical), "no policy: canonical PATH only: {path}");
    }
    let policy = sb.policy(|_| {});
    let path = render(Some(&policy));
    let (first, rest) = path.split_once(':').unwrap();
    assert_eq!(Path::new(first), sb.path(), "launcher dir first: {path}");
    assert!(rest.starts_with(&canonical), "the canonical set follows: {path}");
}
