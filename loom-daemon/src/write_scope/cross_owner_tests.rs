//! #12606: a write to a repository owned by another account than the daemon
//! workspace root's is vetted under the credential it carries.
//!
//! The fleet shape: the daemon process runs under the workspace-root owner's
//! writer (`GH_CONFIG_DIR` = owner A's), and every managed checkout of another
//! owner B is registered against B's per-owner writer (#5401 / #5431). The
//! `gh` facade sends a write from B's checkout under B's writer
//! (`GhInvocation::env_plan`). The #9548 gate in front of the daemon's
//! in-process `GhForge` writes (the Champion re-evaluation ladder, dependency
//! parks, step refusals) used to probe the *process* credential instead, so
//! every such write was refused "not in this App installation" although the
//! write itself would have succeeded.
//!
//! The fake `gh` here answers like GitHub does for App installation tokens:
//! B's writer lists B's repository in `installation/repositories`, A's writer
//! does not and gets a 404 for B's repository. It logs `GH_CONFIG_DIR` and
//! argv for every call, so the tests can say which credential each call ran
//! under.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use super::*;
use crate::operator_decision::cli::GhForge;
use crate::step_dispatch::reeval_ladder;
use crate::step_role::StepRole;
use crate::telemetry::kinds::write_scope_refused::RefusalClass;

const OWNER_B: &str = "xowner-b-12606";

/// Set process environment variables for one test and restore them on drop.
struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    fn set(vars: &[(&'static str, Option<&std::ffi::OsStr>)]) -> Self {
        let mut prior = Vec::new();
        for (k, v) in vars {
            prior.push((*k, std::env::var_os(k)));
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
        Self(prior)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in self.0.drain(..) {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }
}

/// The two-owner host: A's writer as the process credential, B's checkout
/// registered against B's writer, and the fake `gh`.
struct TwoOwners {
    _tmp: tempfile::TempDir,
    _env: EnvGuard,
    root_b: PathBuf,
    repo_b: String,
    a_cfg: PathBuf,
    b_cfg: PathBuf,
    log: PathBuf,
}

impl TwoOwners {
    /// `b_writes`: whether B's writer installation covers B's repository (the
    /// negative control turns it off).
    fn new(register_root: bool, b_writes: bool) -> Self {
        crate::credential_preflight::clear_owner_root_registry();
        probe::clear_memory();
        let tmp = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().simple().to_string();
        let repo_b = format!("{OWNER_B}/r{}", &id[..10]);
        let a_cfg = tmp.path().join("gh-config-owner-a");
        let b_cfg = tmp.path().join("gh-config-by-owner").join(OWNER_B);
        std::fs::create_dir_all(&a_cfg).unwrap();
        std::fs::create_dir_all(&b_cfg).unwrap();
        let root_b = tmp.path().join("checkout-b");
        std::fs::create_dir_all(root_b.join(".loom")).unwrap();
        git(&root_b, &["init", "-q"]);
        git(
            &root_b,
            &[
                "remote",
                "add",
                "origin",
                &format!("https://github.com/{repo_b}.git"),
            ],
        );
        if register_root {
            crate::credential_preflight::register_root_gh_config_dir(&root_b, &b_cfg);
        }
        crate::credential_preflight::register_owner_gh_config_dir(OWNER_B, &b_cfg);

        let log = tmp.path().join("gh.log");
        let listing = if b_writes {
            repo_b.clone()
        } else {
            format!("{OWNER_B}/some-other-repo")
        };
        let gh = tmp.path().join("gh");
        let script = format!(
            "#!/bin/sh\n\
             printf '%s|%s\\n' \"$GH_CONFIG_DIR\" \"$*\" >> '{log}'\n\
             if [ \"$GH_CONFIG_DIR\" = '{b}' ]; then\n\
               case \"$*\" in\n\
                 *installation/repositories*) echo '{listing}'; exit 0;;\n\
                 'api repos/{repo} --jq'*) echo '{{}}'; exit 0;;\n\
                 'api repos/{repo}/issues/7') echo '{{\"body\":\"held\",\"labels\":[]}}'; exit 0;;\n\
                 'api -X PATCH repos/{repo}/issues/7'*) echo '{{}}'; exit 0;;\n\
               esac\n\
             fi\n\
             case \"$*\" in\n\
               *installation/repositories*) echo 'someone-else/unrelated'; exit 0;;\n\
             esac\n\
             echo 'gh: Not Found (HTTP 404)' >&2\n\
             exit 1\n",
            log = log.display(),
            b = b_cfg.display(),
            repo = repo_b,
        );
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let cache = tmp.path().join("write-scope-cache");
        let env = EnvGuard::set(&[
            ("GH_CONFIG_DIR", Some(a_cfg.as_os_str())),
            ("LOOM_GH_BIN", Some(gh.as_os_str())),
            ("LOOM_WRITE_SCOPE_CACHE_DIR", Some(cache.as_os_str())),
            ("LOOM_INSTALLATION_SNAPSHOT", Some(std::ffi::OsStr::new("0"))),
            ("GH_TOKEN", None),
            ("GITHUB_TOKEN", None),
            ("GH_REPO", None),
            ("LOOM_REPO", None),
        ]);
        Self {
            _tmp: tmp,
            _env: env,
            root_b,
            repo_b,
            a_cfg,
            b_cfg,
            log,
        }
    }

    /// `(GH_CONFIG_DIR, argv)` of every `gh` call so far.
    fn calls(&self) -> Vec<(String, String)> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('|'))
            .map(|(d, a)| (d.to_string(), a.to_string()))
            .collect()
    }

    fn dir(p: &Path) -> String {
        p.display().to_string()
    }
}

impl Drop for TwoOwners {
    fn drop(&mut self) {
        crate::credential_preflight::clear_owner_root_registry();
        probe::clear_memory();
    }
}

fn git(dir: &Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success();
    assert!(ok, "git {args:?}");
}

/// The fleet case end to end: the Champion ladder marker written through the
/// daemon's `GhForge` from B's checkout. The gate probes B's writer and
/// admits the write, and the PATCH itself runs under B's writer. A's writer
/// (the process credential) is never asked about B's repository, and no
/// other credential (a reader's included) carries the write.
#[test]
#[serial_test::serial]
fn the_ladder_write_on_a_cross_owner_repo_runs_and_is_vetted_under_the_item_owners_writer() {
    for register_root in [true, false] {
        let h = TwoOwners::new(register_root, true);
        let mut forge = GhForge::new(h.root_b.clone(), Some(h.repo_b.clone()))
            .with_call_site("step_ledger.reeval_ladder");
        let rec = reeval_ladder::record(&mut forge, 7, StepRole::ChampionMerge, chrono::Utc::now());
        assert!(rec.is_ok(), "register_root={register_root}: {rec:?}");

        let calls = h.calls();
        let b = TwoOwners::dir(&h.b_cfg);
        let patches: Vec<_> = calls.iter().filter(|(_, a)| a.contains("PATCH")).collect();
        assert_eq!(patches.len(), 1, "{calls:?}");
        assert_eq!(patches[0].0, b, "the write carries B's writer: {calls:?}");
        let probes: Vec<_> = calls
            .iter()
            .filter(|(_, a)| {
                a.contains("installation/repositories")
                    || a.starts_with(&format!("api repos/{} --jq", h.repo_b))
            })
            .collect();
        assert!(!probes.is_empty(), "the gate probed: {calls:?}");
        for (dir, argv) in &probes {
            assert_eq!(dir, &b, "the probe `{argv}` ran under the write's credential: {calls:?}");
        }
        let a = TwoOwners::dir(&h.a_cfg);
        assert!(
            calls.iter().all(|(dir, _)| dir == &b),
            "nothing ran under A's writer ({a}) or any other credential: {calls:?}"
        );
    }
}

/// The gate's own decision names the credential it probed: B's writer for
/// B's checkout, whether the checkout or only B's owner slug is registered.
#[test]
#[serial_test::serial]
fn may_write_from_probes_the_credential_the_write_carries() {
    let h = TwoOwners::new(true, true);
    let (d, dir) = may_write_decision(&h.root_b, Some(&h.repo_b), &default_gh());
    assert_eq!(d.verdict, Verdict::Allow(h.repo_b.clone()));
    assert_eq!(dir.as_deref(), Some(h.b_cfg.as_path()));
    // No explicit repo: the write's target is the checkout's origin and its
    // credential the checkout's registration.
    let (d, dir) = may_write_decision(&h.root_b, None, &default_gh());
    assert_eq!(d.verdict, Verdict::Allow(h.repo_b.clone()));
    assert_eq!(dir.as_deref(), Some(h.b_cfg.as_path()));
}

/// Nothing registered (a CLI process, a single-owner host): the gate keeps
/// probing this process's own credential, as before.
#[test]
#[serial_test::serial]
fn with_nothing_registered_the_process_credential_is_probed() {
    let h = TwoOwners::new(false, true);
    crate::credential_preflight::clear_owner_root_registry();
    let (d, dir) = may_write_decision(&h.root_b, Some(&h.repo_b), &default_gh());
    assert_eq!(dir, None);
    assert!(
        matches!(&d.verdict, Verdict::Deny(w) if w.contains(probe::NOT_IN_INSTALLATION)),
        "A's writer does not cover B's repository: {:?}",
        d.verdict
    );
    let a = TwoOwners::dir(&h.a_cfg);
    assert!(h.calls().iter().all(|(dir, _)| dir == &a), "{:?}", h.calls());
}

/// A refused `GhForge` write is exported: one `write_scope.refused` record
/// naming the call site, the repository and its owner, and the credential and
/// whose writer it is, so a cross-owner refusal is visible in telemetry.
#[test]
#[serial_test::serial]
fn a_refused_ladder_write_exports_a_record_with_both_owners() {
    let h = TwoOwners::new(true, false);
    let ((), captured) = crate::observability::ops::capture::capture(|| {
        let mut forge = GhForge::new(h.root_b.clone(), Some(h.repo_b.clone()))
            .with_call_site("step_ledger.reeval_ladder");
        let err = reeval_ladder::record(&mut forge, 7, StepRole::ChampionMerge, chrono::Utc::now())
            .unwrap_err();
        assert!(err.error.contains("refusing the write (#9548)"), "{}", err.error);
    });
    let records: Vec<_> = captured
        .records
        .into_iter()
        .filter_map(|r| match r {
            crate::telemetry::TelemetryRecord::WriteScopeRefused(r) => Some(r),
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), 1, "{records:?}");
    let r = &records[0];
    assert_eq!(r.call_site, "step_ledger.reeval_ladder");
    assert_eq!(r.repo.as_deref(), Some(h.repo_b.as_str()));
    assert_eq!(r.repo_owner.as_deref(), Some(OWNER_B));
    assert_eq!(r.credential, format!("gh-config:{OWNER_B}"));
    assert_eq!(r.credential_owner.as_deref(), Some(OWNER_B));
    assert_eq!(r.reason, RefusalClass::NotInInstallation);
    assert!(
        h.calls().iter().all(|(_, a)| !a.contains("PATCH")),
        "a refused write is never sent"
    );
}

/// The per-call checks export too, with the process credential named as
/// `process` when nothing is registered.
#[test]
#[serial_test::serial]
fn a_refused_may_write_from_names_the_process_credential() {
    let h = TwoOwners::new(false, true);
    crate::credential_preflight::clear_owner_root_registry();
    let (v, captured) = crate::observability::ops::capture::capture(|| {
        may_write_from_at(&h.root_b, Some(&h.repo_b), "test.site")
    });
    assert!(!v.is_allowed());
    let r = captured
        .records
        .into_iter()
        .find_map(|r| match r {
            crate::telemetry::TelemetryRecord::WriteScopeRefused(r) => Some(r),
            _ => None,
        })
        .expect("a record");
    assert_eq!(r.call_site, "test.site");
    assert_eq!(r.credential, "gh-process-default");
    assert_eq!(r.credential_owner.as_deref(), Some("process"));
    assert_eq!(r.repo_owner.as_deref(), Some(OWNER_B));
}

/// The probe's reads are read-for-write: pinned to the credential under test,
/// never reader-only, and that credential is not set aside for routing
/// (#12564 / #12588 make every other daemon read reader-only).
#[test]
fn the_probe_reads_stay_on_the_gated_writes_credential() {
    let dir = PathBuf::from("/tmp/gh-config-by-owner/xowner-b-12606");
    let p = probe::GhProbe::new(PathBuf::from("gh"), Some(dir.clone()));
    for args in [
        &["repos/a/b", "--jq", ".permissions // {}"][..],
        &["installation/repositories", "--paginate"][..],
    ] {
        let inv = p.invocation(args);
        assert!(!inv.is_reader_only(), "{args:?} is pinned to the writer");
        let inv = inv.route_site_credential();
        assert!(!inv.has_site_credential_set_aside(), "{args:?} keeps its credential");
        let plan = inv.env_plan(None);
        let cfg = plan
            .iter()
            .find(|e| e.key == "GH_CONFIG_DIR")
            .and_then(|e| e.value.clone());
        assert_eq!(cfg, Some(dir.clone().into_os_string()), "{args:?}");
    }
}
