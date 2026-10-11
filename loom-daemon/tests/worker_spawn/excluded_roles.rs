//! #11400: a model profile's `excludedRoles` is enforced at the native launch
//! seam too — `zai-flash` (the bundled default) never runs Judge or Curator,
//! whether the profile came from the default, `LOOM_MODEL_PROFILE` or an
//! explicit `--profile`.
use super::worker;

fn roles(root: &std::path::Path) {
    let roles = root.join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    for role in ["builder", "judge", "curator", "doctor"] {
        std::fs::write(roles.join(format!("{role}.json")), "{}").unwrap();
        std::fs::write(roles.join(format!("{role}.md")), "Role task: $ARGUMENTS").unwrap();
    }
}

fn refused(out: &std::process::Output, role: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{role}: {stderr}");
    assert!(stderr.contains("excludedRoles"), "{role}: {stderr}");
    assert!(stderr.contains("zai-flash"), "{role}: {stderr}");
    assert!(out.stdout.is_empty(), "{role}: the harness must never have started");
}

#[test]
fn judge_and_curator_never_launch_on_zai_flash() {
    let d = tempfile::tempdir().unwrap();
    roles(d.path());
    for runtime in ["pi", "opencode"] {
        for role in ["judge", "curator"] {
            // The bundled default profile, via a role slash prompt.
            let out = worker(d.path(), runtime)
                .args(["-p", &format!("/loom:{role} 1")])
                .output()
                .unwrap();
            refused(&out, role);
            // LOOM_MODEL_PROFILE pin, via the role-tagged launch.
            let out = worker(d.path(), runtime)
                .env("LOOM_ROLE", role)
                .env("LOOM_MODEL_PROFILE", "zai-flash")
                .args(["-p", "hello"])
                .output()
                .unwrap();
            refused(&out, role);
            // Admission judged the env profile; an explicit --profile that
            // differs is re-checked against what was actually selected.
            let out = worker(d.path(), runtime)
                .env("LOOM_ROLE", role)
                .env("LOOM_MODEL_PROFILE", "quick-flash")
                .args(["--profile", "zai-flash", "-p", "hello"])
                .output()
                .unwrap();
            refused(&out, role);
        }
        // Doctor is not excluded: the same launch goes through.
        let out = worker(d.path(), runtime)
            .env("LOOM_ROLE", "doctor")
            .args(["--profile", "zai-flash", "-p", "hello"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{runtime}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

/// The effective launch profile is what admission judges: an explicit allowed
/// `--profile` launches despite an excluding default or `LOOM_MODEL_PROFILE`.
#[test]
fn explicit_allowed_profile_overrides_an_excluding_default_or_env() {
    let d = tempfile::tempdir().unwrap();
    roles(d.path());
    for runtime in ["pi", "opencode"] {
        for role in ["judge", "curator"] {
            // Bundled zai-flash default, explicit quick-flash, role slash prompt.
            let out = worker(d.path(), runtime)
                .args(["--profile", "quick-flash", "-p", &format!("/loom:{role} 1")])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{runtime} {role}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            // Excluding env pin, explicit allowed profile, role-tagged launch.
            let out = worker(d.path(), runtime)
                .env("LOOM_ROLE", role)
                .env("LOOM_MODEL_PROFILE", "zai-flash")
                .args(["--profile", "quick-flash", "-p", "hello"])
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{runtime} {role}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        // A native sweep runs Judge/Curator in-session: same override applies...
        let out = worker(d.path(), runtime)
            .args(["--profile", "quick-flash", "-p", "/loom:sweep 1"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{runtime} sweep: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        // ...and the opposite override is still refused.
        let out = worker(d.path(), runtime)
            .args(["--profile", "zai-flash", "-p", "/loom:sweep 1"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(78), "{runtime} sweep");
        assert!(String::from_utf8_lossy(&out.stderr).contains("excludedRoles"));
    }
}
