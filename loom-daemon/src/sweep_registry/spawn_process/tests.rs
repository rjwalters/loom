//! Service-tier environment at the shared fresh and resumed child boundary.

use super::*;
use crate::env_test_support::EnvGuard;
use crate::runtime_admission::{resolve_and_admit, ResolvedRuntime};
use crate::step_role::StepRole;
use crate::sweep_registry::resume_handle::RollResumeLaunch;
use crate::sweep_registry::test_support::{
    install_runtime_admission_fixture, lifecycle_registry, write_config,
};
use serial_test::serial;

const SERVICE_TIER: &str = "LOOM_SERVICE_TIER";
const BUILDER: SweepKind = SweepKind::Step {
    item: 14504,
    role: StepRole::Builder,
};

fn fixture(root: &Path) -> (SweepRegistry, ResolvedRuntime) {
    let registry = lifecycle_registry(
        root,
        "#!/bin/sh\nprintf 'tier=%s\\nresume=%s\\n' \"${LOOM_SERVICE_TIER-unset}\" \"${LOOM_RESUME_SESSION_ID-unset}\"\n",
    );
    install_runtime_admission_fixture(root);
    let admission = resolve_and_admit(root, "builder", Some("claude")).unwrap();
    (registry, admission)
}

fn child_env(
    registry: &SweepRegistry,
    kind: &SweepKind,
    admission: &ResolvedRuntime,
    resume: Option<&RollResumeLaunch>,
) -> String {
    let log = tempfile::NamedTempFile::new_in(&registry.config.workspace_root).unwrap();
    let (mut child, _) = registry
        .spawn_child_process_for(
            kind,
            log.path(),
            "step-builder-14504-test",
            None,
            None,
            None,
            Some(admission),
            None,
            resume,
        )
        .unwrap();
    assert!(child.wait().unwrap().success());
    std::fs::read_to_string(log.path()).unwrap()
}

#[test]
#[serial]
fn admitted_step_exports_the_configured_service_tier_on_fresh_and_resumed_launches() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, admission) = fixture(dir.path());
    let _ambient = EnvGuard::set(SERVICE_TIER, "inherited");
    write_config(
        dir.path(),
        r#"{"autonomous":{"roleRunner":{"serviceTier":"default","roleServiceTiers":{
            "builder":"standard","builder@codex":"priority","builder@claude":"fast"
        }}}}"#,
    );
    let fresh = child_env(&registry, &BUILDER, &admission, None);
    assert!(fresh.contains("tier=fast\nresume=unset\n"), "{fresh}");
    // Roll-resume admission uses the sweep lifecycle role. The named step
    // still supplies the role key for the service tier.
    let resumed_admission =
        resolve_and_admit(dir.path(), "sweep-lifecycle", Some("claude")).unwrap();
    let resume = RollResumeLaunch {
        runtime: resumed_admission.runtime.clone(),
        session_id: "4910f978-64b9-4654-942a-dae6514e859c".into(),
        ..RollResumeLaunch::default()
    };
    let resumed = child_env(&registry, &BUILDER, &resumed_admission, Some(&resume));
    assert!(
        resumed.contains(&format!("tier=fast\nresume={}\n", resume.session_id)),
        "{resumed}"
    );

    write_config(
        dir.path(),
        r#"{"autonomous":{"roleRunner":{"serviceTier":"default","roleServiceTiers":{"builder":"standard"}}}}"#,
    );
    let bare = child_env(&registry, &BUILDER, &admission, None);
    assert!(bare.contains("tier=standard\n"), "{bare}");

    write_config(dir.path(), r#"{"autonomous":{"roleRunner":{"serviceTier":"default"}}}"#);
    let fallback = child_env(&registry, &BUILDER, &admission, None);
    assert!(fallback.contains("tier=default\n"), "{fallback}");
}

#[test]
#[serial]
fn unmatched_steps_and_roleless_sweeps_keep_service_tier_inheritance() {
    let dir = tempfile::tempdir().unwrap();
    let (registry, admission) = fixture(dir.path());
    let _ambient = EnvGuard::set(SERVICE_TIER, "inherited");
    let inherited = child_env(&registry, &BUILDER, &admission, None);
    assert!(inherited.contains("tier=inherited\n"), "{inherited}");

    write_config(dir.path(), r#"{"autonomous":{"roleRunner":{"serviceTier":"fast"}}}"#);
    for kind in [
        SweepKind::Issue(14504),
        SweepKind::PrSet(vec![14504, 14505]),
    ] {
        let inherited = child_env(&registry, &kind, &admission, None);
        assert!(inherited.contains("tier=inherited\n"), "{inherited}");
    }

    let _no_ambient = EnvGuard::clear(&[SERVICE_TIER]);
    write_config(dir.path(), "{}");
    let unset = child_env(&registry, &BUILDER, &admission, None);
    assert!(unset.contains("tier=unset\n"), "{unset}");
}
