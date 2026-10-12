//! Native sweep launches through `spawn-worker` (moved out of
//! `worker_spawn.rs` to keep it under the file-size threshold, #11400).
use super::{config, worker};

#[test]
fn native_sweep_prompt_uses_sequential_cli_lifecycle_and_fails_closed_tools() {
    let d = tempfile::tempdir().unwrap();
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    // A native sweep runs Judge/Curator in-session; the bundled default
    // profile (`zai-flash`) excludes both (#11400), so pin one that does not.
    config(d.path(), serde_json::json!({"runtimes":{"defaultModelProfile":"quick-flash"}}));
    std::fs::write(
        roles.join("builder.json"),
        r#"{"runtimeRequirements":["loomControl","worktreeIsolation"]}"#,
    )
    .unwrap();
    for role in ["curator", "judge", "doctor"] {
        std::fs::write(
            roles.join(format!("{role}.json")),
            r#"{"runtimeRequirements":["loomControl"]}"#,
        )
        .unwrap();
    }
    for runtime in ["pi", "opencode"] {
        let out = worker(d.path(), runtime)
            .args(["-p", "/loom:sweep 8399 --claim-owned 8399"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("8399 --claim-owned 8399"));
        assert!(text.contains("Native sequential issue lifecycle"));
        assert!(text.contains("post-verdict.sh"));
        assert!(text.contains("merge-pr.sh"));
        if runtime == "pi" {
            assert!(text.contains("--no-builtin-tools"));
        } else {
            assert!(text.contains("loom-worker"));
        }
    }
}

#[test]
fn native_sweep_cannot_bypass_a_phase_capability_requirement() {
    let d = tempfile::tempdir().unwrap();
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    // A native sweep runs Judge/Curator in-session; the bundled default
    // profile (`zai-flash`) excludes both (#11400), so pin one that does not.
    config(d.path(), serde_json::json!({"runtimes":{"defaultModelProfile":"quick-flash"}}));
    for role in ["builder", "curator", "doctor"] {
        std::fs::write(roles.join(format!("{role}.json")), "{}").unwrap();
    }
    std::fs::write(roles.join("judge.json"), r#"{"runtimeRequirements":["mcp"]}"#).unwrap();
    for runtime in ["pi", "opencode"] {
        let out = worker(d.path(), runtime)
            .args(["-p", "/loom:sweep 1"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(78));
        assert!(String::from_utf8_lossy(&out.stderr).contains("native sweep phase judge"));
        assert!(out.stdout.is_empty());
    }
}
