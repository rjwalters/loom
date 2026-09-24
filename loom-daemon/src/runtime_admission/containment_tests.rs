//! Execution-specific admission with verified private-clone containment (#8787).
use super::*;

/// Real shipped role manifests and runtime manifests, with executable adapter
/// stubs — admission behaves as in production, including Codex's native
/// `worktreeIsolation: "partial"`.
fn shipped() -> tempfile::TempDir {
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
    dir
}

struct NoRuntimeEnv(Vec<(&'static str, Option<String>)>);
impl NoRuntimeEnv {
    fn new() -> Self {
        let keys = [
            "LOOM_RUNTIME",
            "LOOM_RUNTIME_BUILDER",
            "LOOM_RUNTIME_DOCTOR",
            "LOOM_RUNTIME_JUDGE",
            "LOOM_RUNTIME_SWEEP_LIFECYCLE",
        ];
        let prior = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
        for key in keys {
            std::env::remove_var(key);
        }
        Self(prior)
    }
}
impl Drop for NoRuntimeEnv {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
#[serial_test::serial]
fn mutable_roles_are_admitted_only_with_a_proof_and_record_provenance() {
    let _env = NoRuntimeEnv::new();
    let dir = shipped();
    let proof = ContainmentProof::fixture("seat-a");
    for role in ["builder", "doctor", "sweep-lifecycle", "pr-fixer", "sweep"] {
        let host = resolve_and_admit(dir.path(), role, Some("codex")).unwrap_err();
        assert_eq!(host.unmet_capabilities, vec!["worktreeIsolation"], "{role}");
        assert!(host.containment_eligible(), "{role}");

        let admitted = resolve_and_admit_in(
            dir.path(),
            role,
            Some("codex"),
            AdmissionContext::PrivateClone(&proof),
        )
        .unwrap();
        assert_eq!(admitted.runtime, "codex");
        // An explicit pin stays a pin: containment does not change the source.
        assert_eq!(admitted.source, RuntimeSource::Explicit);
        let execution = admitted.execution.expect("containment provenance recorded");
        assert_eq!(execution.mode, "private-clone");
        assert_eq!(execution.satisfied, vec!["worktreeIsolation"]);
        // Native values are recorded as they are: this is not hook parity.
        assert_eq!(execution.native["worktreeIsolation"], "partial");
        assert_eq!(execution.native["hooks"], "partial");
        assert_eq!(execution.account, "seat-a");
        assert_eq!(execution.container.len(), 12);
        assert_eq!(execution.protocol, crate::tokens_pool::private_workspace::PROTOCOL);
        let summary = execution.summary();
        assert!(summary.contains("native=hooks=partial,worktreeIsolation=partial"), "{summary}");
    }
}

#[test]
#[serial_test::serial]
fn read_roles_keep_their_requirements_and_carry_no_containment_provenance() {
    let _env = NoRuntimeEnv::new();
    let dir = shipped();
    let proof = ContainmentProof::fixture("seat-a");
    for role in ["judge", "curator", "champion"] {
        let host = resolve_and_admit(dir.path(), role, Some("codex")).unwrap();
        let contained = resolve_and_admit_in(
            dir.path(),
            role,
            Some("codex"),
            AdmissionContext::PrivateClone(&proof),
        )
        .unwrap();
        assert_eq!(host, contained, "{role}: nothing was satisfied by containment");
        assert!(contained.execution.is_none());
    }
}

#[test]
#[serial_test::serial]
fn every_other_requirement_is_still_checked_with_a_proof() {
    let _env = NoRuntimeEnv::new();
    let dir = shipped();
    // A Codex manifest that also lacks loomControl: containment can only ever
    // stand in for repository isolation.
    fs::write(
        dir.path().join(".loom/runtimes/codex.json"),
        r#"{"runtime":"codex","capabilities":{"worktreeIsolation":"partial","loomControl":"no","hooks":"partial"}}"#,
    )
    .unwrap();
    let proof = ContainmentProof::fixture("seat-a");
    let rejected = resolve_and_admit_in(
        dir.path(),
        "builder",
        Some("codex"),
        AdmissionContext::PrivateClone(&proof),
    )
    .unwrap_err();
    assert_eq!(rejected.unmet_capabilities, vec!["loomControl"]);
    let host = resolve_and_admit(dir.path(), "builder", Some("codex")).unwrap_err();
    assert!(!host.containment_eligible(), "two unmet capabilities are never eligible");
}

#[test]
#[serial_test::serial]
fn a_codex_proof_cannot_satisfy_another_runtime() {
    let _env = NoRuntimeEnv::new();
    let dir = shipped();
    fs::write(
        dir.path().join(".loom/runtimes/claude.json"),
        r#"{"runtime":"claude","capabilities":{"worktreeIsolation":"partial","loomControl":"yes"}}"#,
    )
    .unwrap();
    let proof = ContainmentProof::fixture("seat-a");
    let rejected = resolve_and_admit_in(
        dir.path(),
        "builder",
        Some("claude"),
        AdmissionContext::PrivateClone(&proof),
    )
    .unwrap_err();
    assert_eq!(rejected.unmet_capabilities, vec!["worktreeIsolation"]);
    assert!(!rejected.containment_eligible());
}

#[test]
fn shipped_codex_capabilities_are_not_promoted_by_this_feature() {
    let manifest: serde_json::Value =
        serde_json::from_str(bundled_runtime_manifest("codex").unwrap()).unwrap();
    assert_eq!(manifest["capabilities"]["worktreeIsolation"], "partial");
    assert_eq!(manifest["capabilities"]["hooks"], "partial");
}

#[test]
fn containment_failure_keeps_the_unmet_capability_and_names_the_obligation() {
    let rejection = RuntimeRejection {
        role: "builder".into(),
        runtime: "codex".into(),
        source: RuntimeSource::Explicit,
        unmet_capabilities: vec!["worktreeIsolation".into()],
        reason: "unmet capabilities: worktreeIsolation".into(),
    }
    .with_containment_failure("hook not trusted");
    assert_eq!(rejection.unmet_capabilities, vec!["worktreeIsolation"]);
    assert!(rejection
        .reason
        .starts_with("unmet capabilities: worktreeIsolation"));
    assert!(rejection.reason.contains("hook not trusted"));
    assert!(rejection.diagnostic().contains("hook not trusted"));
}
