//! Library-test fixture: a hermetic runtime-selection environment (#9360).
//!
//! [`crate::runtime_admission::resolve_binding`] is the first decision in every
//! dispatch, and it is also what classifies the default-model branch behind
//! [`crate::sweep_registry::resolve_dispatch_model`]. Before it reads any
//! config file it reads two **process-global** environment variables:
//!
//! - `LOOM_RUNTIME` — the global runtime override, and
//! - `LOOM_RUNTIME_<ROLE>` — the per-role override (`LOOM_RUNTIME_JUDGE`, …).
//!
//! Both outrank everything a test fixture can write into its tempdir, which
//! makes any test that drives admission or model resolution a function of the
//! shell it was launched from.
//!
//! That shell is not always clean. A Loom agent session is spawned with the
//! admitted runtime pinned into its environment on purpose
//! ([`crate::launch_env::apply_launch_env`], so `spawn-worker.sh` cannot
//! re-resolve a different runtime after the pre-spawn decision), so a unit-test
//! run from *inside* a session — exactly what `buildGate` does — inherits e.g.
//! `LOOM_RUNTIME=opencode` on a native dispatch worker. Issue #9360: three
//! tests failed that way on a native worker (two reporting a missing `--model`
//! pin, one getting `RuntimeRejected { runtime: "opencode", … }` instead of the
//! refusal it asserts) while the same commits were green on CI's clean runners,
//! stalling the gate regardless of the code under test.
//!
//! [`ClearedRuntimeSelectionEnv`] removes every variable `resolve_binding`
//! reads and restores the prior values on drop. Reach for it in any test that
//! asserts on a runtime binding, an admission outcome, or a resolved dispatch
//! model, and hold a `serial_test` key while it is alive — the variables are
//! process-global, so a concurrent test would observe the removal.

use std::ffi::{OsStr, OsString};
use std::path::Path;

/// Is `name` one of the environment variables
/// [`crate::runtime_admission::resolve_binding`] reads?
///
/// The per-role name it builds is
/// `LOOM_RUNTIME_{canonical.replace('-', "_").to_ascii_uppercase()}`, so the
/// suffix is handed straight back to the real
/// [`crate::runtime_admission::canonical_role`] rather than matched against a
/// second copy of the role list that could drift from it.
///
/// `LOOM_RUNTIME_PREFERENCE_MARKER`
/// ([`crate::launch_env::PREFERENCE_MARKER_ENV`]) deliberately does **not**
/// match: it carries an ordered-preference log marker, not a runtime
/// selection, and nothing in admission reads it.
fn is_runtime_selection_var(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    if name == "LOOM_RUNTIME" {
        return true;
    }
    name.strip_prefix("LOOM_RUNTIME_")
        .is_some_and(|role| crate::runtime_admission::canonical_role(role).is_some())
}

/// RAII guard that clears every runtime-selection environment variable for the
/// scope of a test and restores exactly what was there before — including
/// across a mid-test assertion panic, since Rust unwinds through `Drop`.
///
/// Supersedes the per-file `ClearedLoomRuntimeEnv` copies (#4739/#6529), which
/// cleared only the global `LOOM_RUNTIME` and left a `LOOM_RUNTIME_<ROLE>` pin
/// able to decide the same binding.
pub(crate) struct ClearedRuntimeSelectionEnv(Vec<(OsString, OsString)>);

impl ClearedRuntimeSelectionEnv {
    pub(crate) fn new() -> Self {
        let prior: Vec<(OsString, OsString)> = std::env::vars_os()
            .filter(|(name, _)| is_runtime_selection_var(name))
            .collect();
        for (name, _) in &prior {
            std::env::remove_var(name);
        }
        Self(prior)
    }
}

impl Drop for ClearedRuntimeSelectionEnv {
    fn drop(&mut self) {
        for (name, value) in self.0.drain(..) {
            std::env::set_var(name, value);
        }
    }
}

/// Install the minimal `.loom/` surface that admits a zero-config Claude
/// dispatch under `root`: a `builder` role manifest requiring the two
/// capabilities, a `claude` runtime manifest declaring them, and an executable
/// `spawn-claude.sh` adapter.
///
/// Runtime admission is the first dispatch decision, so a fixture that asserts
/// on a *later* decision needs this much installed or it never gets there —
/// pair it with [`ClearedRuntimeSelectionEnv`] so an ambient pin cannot send
/// admission at a runtime this surface does not describe.
///
/// # Panics
/// On any filesystem error — this is a test fixture.
pub(crate) fn install_admissible_claude_surface(root: &Path) {
    for leaf in ["roles", "runtimes", "scripts"] {
        std::fs::create_dir_all(root.join(".loom").join(leaf)).unwrap();
    }
    std::fs::write(
        root.join(".loom/roles/builder.json"),
        r#"{"runtimeRequirements":["worktreeIsolation","mcp"]}"#,
    )
    .unwrap();
    std::fs::write(
        root.join(".loom/runtimes/claude.json"),
        r#"{"runtime":"claude","capabilities":{"worktreeIsolation":"yes","mcp":"yes"}}"#,
    )
    .unwrap();
    let adapter = root.join(".loom/scripts/spawn-claude.sh");
    std::fs::write(&adapter, "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn matches_exactly_what_resolve_binding_reads() {
        for name in [
            "LOOM_RUNTIME",
            "LOOM_RUNTIME_SWEEP_LIFECYCLE",
            "LOOM_RUNTIME_JUDGE",
            "LOOM_RUNTIME_BUILDER",
        ] {
            assert!(is_runtime_selection_var(OsStr::new(name)), "{name} must be cleared");
        }
        for name in [
            // A launch marker, not a selection (#8599).
            crate::launch_env::PREFERENCE_MARKER_ENV,
            "LOOM_RUNTIME_NOT_A_ROLE",
            "LOOM_WORKSPACE",
            "TMPDIR",
        ] {
            assert!(!is_runtime_selection_var(OsStr::new(name)), "{name} must be kept");
        }
    }

    #[test]
    #[serial]
    fn clears_global_and_per_role_pins_then_restores_them() {
        std::env::set_var("LOOM_RUNTIME", "opencode");
        std::env::set_var("LOOM_RUNTIME_JUDGE", "codex");
        std::env::set_var(crate::launch_env::PREFERENCE_MARKER_ENV, "# marker");
        {
            let _guard = ClearedRuntimeSelectionEnv::new();
            assert!(std::env::var_os("LOOM_RUNTIME").is_none());
            assert!(std::env::var_os("LOOM_RUNTIME_JUDGE").is_none());
            assert_eq!(
                std::env::var(crate::launch_env::PREFERENCE_MARKER_ENV).as_deref(),
                Ok("# marker"),
                "a non-selection LOOM_RUNTIME_* var must survive"
            );
        }
        assert_eq!(std::env::var("LOOM_RUNTIME").as_deref(), Ok("opencode"));
        assert_eq!(std::env::var("LOOM_RUNTIME_JUDGE").as_deref(), Ok("codex"));
        std::env::remove_var("LOOM_RUNTIME");
        std::env::remove_var("LOOM_RUNTIME_JUDGE");
        std::env::remove_var(crate::launch_env::PREFERENCE_MARKER_ENV);
    }

    #[test]
    #[serial]
    fn an_absent_pin_stays_absent_after_the_guard_drops() {
        std::env::remove_var("LOOM_RUNTIME");
        {
            let _guard = ClearedRuntimeSelectionEnv::new();
            assert!(std::env::var_os("LOOM_RUNTIME").is_none());
        }
        assert!(
            std::env::var_os("LOOM_RUNTIME").is_none(),
            "the guard must not invent a value it never saw"
        );
    }

    #[test]
    #[serial]
    fn installed_surface_is_admissible_for_a_cleared_environment() {
        let root = tempfile::tempdir().unwrap();
        install_admissible_claude_surface(root.path());
        let _guard = ClearedRuntimeSelectionEnv::new();
        let (runtime, _source) =
            crate::runtime_admission::resolve_binding(root.path(), "builder", None).unwrap();
        assert_eq!(runtime, "claude");
    }
}
