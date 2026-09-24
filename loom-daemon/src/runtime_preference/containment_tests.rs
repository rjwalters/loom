//! The preference walk and pins with a containment preparer (#8787).
//!
//! The preparer here is a fake that admits against a synthetic proof; the
//! proof-producing preparer itself (Docker validation, lease, in-container
//! policy) is exercised by `tests/private_workspace_docker.rs`.
use super::*;
use crate::runtime_admission::{AdmissionContext, ContainmentProof};
use std::fs;

struct Env(Vec<(&'static str, Option<String>)>, tempfile::TempDir);
impl Env {
    fn new() -> Self {
        let keys = [
            "LOOM_RUNTIME",
            "LOOM_RUNTIME_BUILDER",
            "LOOM_RUNTIME_JUDGE",
            "LOOM_RUNTIME_SWEEP_LIFECYCLE",
            "LOOM_CODEX_PROFILE_ROOT",
            "LOOM_CODEX_PROFILE",
            "LOOM_CODEX_HOME",
            "CODEX_HOME",
            "LOOM_SPAWN_NO_EXPORT",
            "LOOM_CODEX_NO_EXEC",
            ceiling::MAX_CONCURRENT_ENV,
        ];
        let prior = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for key in keys {
            std::env::remove_var(key);
        }
        let profiles = tempfile::tempdir().unwrap();
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
        Self(prior, profiles)
    }
    fn provision_codex_seat(&self) {
        fs::create_dir_all(self.1.path().join("seat")).unwrap();
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn workspace(preference: Option<serde_json::Value>) -> tempfile::TempDir {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (from, to) in [
        ("defaults/roles", ".loom/roles"),
        ("defaults/runtimes", ".loom/runtimes"),
    ] {
        fs::create_dir_all(dir.path().join(to)).unwrap();
        for entry in fs::read_dir(repo.join(from)).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                fs::copy(&path, dir.path().join(to).join(path.file_name().unwrap())).unwrap();
            }
        }
    }
    fs::create_dir_all(dir.path().join(".loom/scripts")).unwrap();
    for runtime in ["claude", "codex"] {
        let adapter = dir.path().join(format!(".loom/scripts/spawn-{runtime}.sh"));
        fs::write(&adapter, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(adapter, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config = match preference {
        Some(list) => serde_json::json!({"runtimes": {"preference": list}}),
        None => serde_json::json!({}),
    };
    fs::write(dir.path().join(".loom/config.json"), config.to_string()).unwrap();
    dir
}

/// Admits against a synthetic proof when `verified`; records every call.
struct FakePreparer {
    root: std::path::PathBuf,
    verified: bool,
    proof: ContainmentProof,
    contained: Vec<String>,
    held: bool,
    releases: usize,
}
impl FakePreparer {
    fn new(root: &Path, verified: bool) -> Self {
        Self {
            root: root.to_owned(),
            verified,
            proof: ContainmentProof::fixture("seat"),
            contained: Vec::new(),
            held: false,
            releases: 0,
        }
    }
}
impl ContainmentPreparer for FakePreparer {
    fn contain(
        &mut self,
        role: &str,
        explicit: Option<&str>,
        rejection: RuntimeRejection,
    ) -> Result<ResolvedRuntime, RuntimeRejection> {
        self.contained
            .push(format!("{role}:{}", explicit.unwrap_or("-")));
        if !self.verified {
            return Err(rejection.with_containment_failure("fixture: hook trust unproven"));
        }
        let admitted = crate::runtime_admission::resolve_and_admit_in(
            &self.root,
            role,
            explicit,
            AdmissionContext::PrivateClone(&self.proof),
        )?;
        self.held = true;
        Ok(admitted)
    }
    fn release(&mut self) {
        self.held = false;
        self.releases += 1;
    }
}

fn walk(root: &Path, role: &str, preparer: &mut FakePreparer) -> Decision {
    resolve_runtime_contained(root, role, None, 0, DispatchContext::default(), preparer).unwrap()
}

/// Claude dry -> verified Codex -> metered native backstop: the contained
/// Codex tap is chosen and keeps its prepared selection.
#[test]
#[serial_test::serial]
fn claude_then_verified_codex_then_backstop() {
    let env = Env::new();
    env.provision_codex_seat();
    let dir = workspace(Some(serde_json::json!(["claude", "codex", "opencode"])));
    let mut preparer = FakePreparer::new(dir.path(), true);
    let Decision::Preference { resolution, .. } = walk(dir.path(), "builder", &mut preparer) else {
        panic!("expected the preference path");
    };
    let chosen = resolution.chosen.as_ref().unwrap();
    assert_eq!(chosen.tap.runtime, "codex");
    assert_eq!(chosen.tier, 1);
    assert_eq!(chosen.admitted.source, RuntimeSource::Preference);
    assert!(chosen.admitted.execution.is_some());
    assert_eq!(preparer.contained, vec!["builder:codex"]);
    assert!(preparer.held, "the chosen contained tap keeps its selection");
    assert_eq!(resolution.skipped[0].tap.runtime, "claude");
}

/// A contained candidate that then fails availability is released, and the
/// walk continues to the backstop.
#[test]
#[serial_test::serial]
fn a_contained_tap_passed_over_releases_its_selection() {
    let _env = Env::new(); // no codex seat: the codex pool is empty
    let dir = workspace(Some(serde_json::json!(["claude", "codex", "opencode"])));
    let mut preparer = FakePreparer::new(dir.path(), true);
    let Decision::Preference { resolution, .. } = walk(dir.path(), "builder", &mut preparer) else {
        panic!("expected the preference path");
    };
    assert_eq!(resolution.chosen.as_ref().unwrap().tap.runtime, "opencode");
    assert!(!preparer.held);
    assert!(preparer.releases >= 1);
    assert_eq!(resolution.skipped[1].reason.kind(), "unavailable");
}

/// An unproven containment is a not-admitted skip naming the obligation; the
/// global capability truth is untouched and the walk falls through.
#[test]
#[serial_test::serial]
fn unverified_containment_is_skipped_with_its_obligation() {
    let env = Env::new();
    env.provision_codex_seat();
    let dir = workspace(Some(serde_json::json!(["codex", "opencode"])));
    let mut preparer = FakePreparer::new(dir.path(), false);
    let Decision::Preference { resolution, .. } = walk(dir.path(), "builder", &mut preparer) else {
        panic!("expected the preference path");
    };
    assert_eq!(resolution.chosen.as_ref().unwrap().tap.runtime, "opencode");
    let skip = &resolution.skipped[0];
    assert_eq!(skip.reason.kind(), "not-admitted");
    let SkipReason::NotAdmitted { unmet, detail } = &skip.reason else {
        unreachable!()
    };
    assert_eq!(unmet, &vec!["worktreeIsolation".to_string()]);
    assert!(detail.contains("hook trust unproven"), "{detail}");
}

/// An explicit pin never falls through: a verified pin is admitted on
/// containment, an unverified one is refused outright.
#[test]
#[serial_test::serial]
fn an_explicit_codex_pin_never_falls_through() {
    let env = Env::new();
    env.provision_codex_seat();
    let dir = workspace(Some(serde_json::json!(["claude", "opencode"])));
    let mut refused = FakePreparer::new(dir.path(), false);
    let error =
        resolve_for_dispatch_with(dir.path(), "sweep-lifecycle", Some("codex"), &mut refused)
            .unwrap_err();
    assert_eq!(error.runtime, "codex");
    assert_eq!(error.source, RuntimeSource::Explicit);
    assert!(error.reason.contains("hook trust unproven"), "{}", error.reason);

    let mut verified = FakePreparer::new(dir.path(), true);
    let admission =
        resolve_for_dispatch_with(dir.path(), "sweep-lifecycle", Some("codex"), &mut verified)
            .unwrap();
    let admitted = admission.admitted.unwrap();
    assert_eq!(admitted.runtime, "codex");
    assert_eq!(admitted.source, RuntimeSource::Explicit);
    assert!(admitted.execution.is_some());
    assert!(verified.held);
}

/// Without a preparer (every probe and lock-held path) behaviour is exactly
/// the pre-#8787 static refusal: no containment is attempted.
#[test]
#[serial_test::serial]
fn without_a_preparer_codex_builder_is_still_refused() {
    let env = Env::new();
    env.provision_codex_seat();
    let dir = workspace(None);
    let error = resolve_for_dispatch(dir.path(), "builder", Some("codex")).unwrap_err();
    assert_eq!(error.unmet_capabilities, vec!["worktreeIsolation"]);
    assert!(!error.reason.contains("containment"), "{}", error.reason);
}

/// Read roles are admitted statically; the preparer is never asked.
#[test]
#[serial_test::serial]
fn read_roles_never_consult_the_preparer() {
    let env = Env::new();
    env.provision_codex_seat();
    let dir = workspace(Some(serde_json::json!(["codex"])));
    let mut preparer = FakePreparer::new(dir.path(), true);
    let Decision::Preference { resolution, .. } = walk(dir.path(), "judge", &mut preparer) else {
        panic!("expected the preference path");
    };
    let chosen = resolution.chosen.unwrap();
    assert_eq!(chosen.tap.runtime, "codex");
    assert!(chosen.admitted.execution.is_none());
    assert!(preparer.contained.is_empty());
}
