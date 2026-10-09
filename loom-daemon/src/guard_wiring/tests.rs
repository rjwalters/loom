//! Fixture tests for the #9108 `PreToolUse` matcher-coverage contract.
//!
//! These are the direct port of the six synthetic fixtures the check carried
//! while it was shell (`check-guard-scan-contracts.sh --self-test`): each one
//! isolates ONE discriminating property, so a fixture that stops failing —
//! or starts failing for a *different* reason — means the checker lost the
//! power it exists for.
//!
//! The last test is the one the fixtures cannot replace: it runs the real
//! contract against **this repository's own tree**, which is what the CI gate
//! does and the only assertion that can catch the wiring actually regressing.

use super::*;
use std::path::PathBuf;

/// Which property a fixture deliberately breaks.
#[derive(Clone, Copy)]
enum Variant {
    Compliant,
    NoMatcher,
    NoFloor,
    BypassWiring,
    NoInstaller,
    NoGuardFile,
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    /// Build a fixture workspace. The settings command string is the real
    /// shape, abbreviated to the properties the contract asserts, and kept to
    /// ONE physical line per hook command exactly as the real settings file
    /// does.
    fn build(variant: Variant) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();

        let matcher = match variant {
            Variant::NoMatcher => "Bash",
            _ => MCP_MATCHER,
        };
        let (launcher, route) = match variant {
            Variant::BypassWiring => ("G=$W/.loom/hooks/guard-mcp-tools.sh;", "exec \\\"$G\\\""),
            _ => (
                "L=$W/.loom/hooks/hook-wiring.sh;",
                "exec bash \\\"$L\\\" PreToolUse guard-mcp-tools.sh",
            ),
        };
        let floor = match variant {
            Variant::NoFloor => "exit 0".to_string(),
            _ => "[ -d \\\"$W/.loom/hooks\\\" ] || exit 0; \
                  [ \\\"${LOOM_GUARD_WIRING_FAILOPEN:-0}\\\" = \\\"1\\\" ] && exit 0; \
                  printf %s '{\\\"hookSpecificOutput\\\":{\\\"permissionDecision\\\":\\\"deny\\\"}}'"
                .to_string(),
        };

        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::create_dir_all(root.join("scripts/install")).unwrap();
        std::fs::create_dir_all(root.join("defaults/hooks")).unwrap();

        let settings = format!(
            "{{ \"hooks\": {{ \"PreToolUse\": [\n\
             \x20 {{ \"matcher\": \"{matcher}\",\n\
             \x20   \"hooks\": [ {{ \"type\": \"command\", \"command\": \"bash -c '{launcher} {route}; {floor}'\" }} ] }}\n\
             ] }} }}\n"
        );
        std::fs::write(root.join(SETTINGS_PATH), settings).unwrap();

        let provision = match variant {
            Variant::NoInstaller => {
                "_PHOOK_MATCHERS=(Bash)\n_PHOOK_NAMES=(guard-destructive.sh)\n".to_string()
            }
            _ => format!(
                "_PHOOK_MATCHERS=(Bash \"{MCP_MATCHER}\")\n_PHOOK_NAMES=(guard-destructive.sh {MCP_GUARD})\n"
            ),
        };
        std::fs::write(root.join(PROVISION_PATH), provision).unwrap();

        if !matches!(variant, Variant::NoGuardFile) {
            std::fs::write(
                root.join(GUARD_SOURCE_DIR).join(MCP_GUARD),
                "#!/usr/bin/env bash\nexit 0\n",
            )
            .unwrap();
        }

        Self { dir }
    }

    fn check(&self) -> Vec<Violation> {
        super::check(self.dir.path())
    }
}

fn headlines(v: &[Violation]) -> Vec<&str> {
    v.iter().map(|x| x.headline.as_str()).collect()
}

#[test]
fn a_compliant_wiring_fixture_passes() {
    let f = Fixture::build(Variant::Compliant);
    let v = f.check();
    assert!(v.is_empty(), "compliant fixture was rejected: {:?}", headlines(&v));
}

#[test]
fn a_settings_file_with_no_mcp_matcher_is_rejected() {
    let f = Fixture::build(Variant::NoMatcher);
    let v = f.check();
    assert!(
        headlines(&v).contains(&"MISSING MCP MATCHER"),
        "expected MISSING MCP MATCHER, got {:?}",
        headlines(&v)
    );
}

#[test]
fn an_mcp_entry_with_no_fail_closed_floor_is_rejected() {
    let f = Fixture::build(Variant::NoFloor);
    let v = f.check();
    assert!(
        headlines(&v)
            .iter()
            .any(|h| h.starts_with("MCP FAIL-CLOSED FLOOR MISSING")),
        "expected a fail-closed floor violation, got {:?}",
        headlines(&v)
    );
}

#[test]
fn an_mcp_entry_that_bypasses_hook_wiring_is_rejected() {
    let f = Fixture::build(Variant::BypassWiring);
    let v = f.check();
    assert!(
        headlines(&v).contains(&"MCP ENTRY BYPASSES hook-wiring.sh"),
        "expected MCP ENTRY BYPASSES hook-wiring.sh, got {:?}",
        headlines(&v)
    );
}

#[test]
fn an_installer_whose_phook_arrays_lack_the_matcher_is_rejected() {
    let f = Fixture::build(Variant::NoInstaller);
    let v = f.check();
    assert!(
        headlines(&v).contains(&"MISSING MCP MATCHER IN INSTALLER"),
        "expected MISSING MCP MATCHER IN INSTALLER, got {:?}",
        headlines(&v)
    );
    assert!(
        headlines(&v).contains(&"MISSING MCP GUARD IN INSTALLER"),
        "expected MISSING MCP GUARD IN INSTALLER, got {:?}",
        headlines(&v)
    );
}

#[test]
fn wiring_that_names_a_guard_file_which_does_not_exist_is_rejected() {
    let f = Fixture::build(Variant::NoGuardFile);
    let v = f.check();
    assert!(
        headlines(&v).contains(&"MISSING MCP GUARD FILE"),
        "expected MISSING MCP GUARD FILE, got {:?}",
        headlines(&v)
    );
}

#[test]
fn an_unreadable_settings_file_is_a_violation_not_a_silent_pass() {
    let dir = tempfile::tempdir().unwrap();
    let v = super::check(dir.path());
    assert_eq!(headlines(&v), vec!["MISSING SETTINGS FILE"]);
}

/// The real thing. A fixture suite proves the checker discriminates; only this
/// proves the repository it guards is actually wired.
#[test]
fn this_repository_satisfies_the_wiring_contract() {
    // CARGO_MANIFEST_DIR is <repo>/loom-daemon.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon has a parent")
        .to_path_buf();
    let v = super::check(&root);
    assert!(
        v.is_empty(),
        "this checkout violates the #9108 wiring contract:\n{}",
        v.iter()
            .map(Violation::render)
            .collect::<Vec<_>>()
            .join("\n\n")
    );
}

/// Write a `guards.enabled:false` opt-out config into `root` (#10335).
fn opt_out_of_guards(root: &Path) {
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(root.join(".loom/config.json"), r#"{"guards":{"enabled":false}}"#).unwrap();
}

/// Issue #10335 review: `guards.enabled:false` opts out of the Bash/Write
/// guards only. The MCP guard is out of scope for it, so an opted-out repo
/// with missing MCP wiring must still fail this contract.
#[test]
fn opted_out_repo_with_missing_mcp_wiring_still_fails() {
    let f = Fixture::build(Variant::NoMatcher);
    opt_out_of_guards(f.dir.path());
    assert!(headlines(&f.check()).contains(&"MISSING MCP MATCHER"));

    let dir = tempfile::tempdir().unwrap();
    opt_out_of_guards(dir.path());
    assert_eq!(headlines(&super::check(dir.path())), vec!["MISSING SETTINGS FILE"]);
}

/// The other half: valid MCP wiring passes without the three opted-out hooks.
#[test]
fn opted_out_repo_with_valid_mcp_wiring_passes() {
    let f = Fixture::build(Variant::Compliant);
    opt_out_of_guards(f.dir.path());
    let v = f.check();
    assert!(v.is_empty(), "opted-out compliant fixture was rejected: {:?}", headlines(&v));
}
