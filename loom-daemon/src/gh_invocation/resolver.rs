//! The single `gh` executable resolver (#9985 slice 1).
//!
//! Precedence, first hit wins:
//!
//! 1. `toolchain.launcherPath` from the resolved forge-egress policy (C1,
//!    #9983). The policy reader has not landed, so [`policy_launcher_path`] is
//!    a stub that always answers `None`; wiring it is a one-function change.
//! 2. `LOOM_GH_BIN` — the test/override hook every existing resolver honours.
//!    Read with `std::env::var` semantics, byte-identical to the ten
//!    hand-rolled `fn gh_bin*` copies this replaces (a set-but-empty value is
//!    returned as-is, a non-UTF-8 value is treated as unset).
//! 3. Bare `"gh"`, resolved from `PATH` by the OS at spawn time.

/// Which rung of the precedence ladder produced the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GhBinSource {
    /// `toolchain.launcherPath` from the forge-egress policy (C1).
    Policy,
    /// The `LOOM_GH_BIN` environment override.
    EnvOverride,
    /// Bare `gh`, looked up on `PATH` at spawn.
    Path,
    /// A program the call site injected ([`super::GhInvocation::program`]) —
    /// a test stub handed down a `gh_bin: &Path` argument (#10089).
    Injected,
}

/// The executable a `gh` invocation will run, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGh {
    pub program: String,
    pub source: GhBinSource,
}

/// Resolve the `gh` executable from the live process state.
#[must_use]
pub fn resolve() -> ResolvedGh {
    resolve_from(policy_launcher_path(), env_override())
}

/// `LOOM_GH_BIN`, and in a unit-test build a loud-failing stub when it is
/// unset (#10088), so `cargo test` can never reach the real `gh` and spend the
/// operator's forge quota.
fn env_override() -> Option<String> {
    let env = std::env::var("LOOM_GH_BIN").ok();
    #[cfg(test)]
    {
        if env.is_none() {
            return Some(test_stub::path().to_string_lossy().into_owned());
        }
    }
    env
}

/// The pure precedence ladder, separated from process state for testing.
#[must_use]
pub fn resolve_from(policy: Option<String>, env_override: Option<String>) -> ResolvedGh {
    if let Some(program) = policy {
        return ResolvedGh {
            program,
            source: GhBinSource::Policy,
        };
    }
    if let Some(program) = env_override {
        return ResolvedGh {
            program,
            source: GhBinSource::EnvOverride,
        };
    }
    ResolvedGh {
        program: "gh".to_string(),
        source: GhBinSource::Path,
    }
}

/// The resolved program name, for the legacy `String`-returning call shape
/// (`forge_cmd::gh_bin()` delegates here).
#[must_use]
pub fn gh_bin() -> String {
    resolve().program
}

/// `toolchain.launcherPath` from the resolved forge-egress policy.
///
/// Stub until the C1 policy reader lands (#9983): no policy source exists in
/// this crate yet, so this always answers `None` and the ladder starts at
/// `LOOM_GH_BIN`. Keeping the rung in place now means the follow-up changes one
/// function, not every caller.
fn policy_launcher_path() -> Option<String> {
    None
}

/// The unit-test-build `gh` stub (#10088).
#[cfg(test)]
pub(crate) mod test_stub {
    use std::path::PathBuf;
    use std::sync::OnceLock;

    static STUB: OnceLock<(tempfile::TempDir, PathBuf)> = OnceLock::new();

    /// Path of a `gh` stand-in that prints the args it was called with and
    /// exits 127, appending them to `$LOOM_GH_STUB_LOG` when set. Written
    /// once per test process.
    pub(crate) fn path() -> PathBuf {
        STUB.get_or_init(|| {
            use std::os::unix::fs::PermissionsExt;
            let dir = tempfile::tempdir().expect("stub tempdir");
            let p = dir.path().join("gh");
            let script = "#!/bin/sh\n\
                echo \"loom-daemon test reached the real gh: $*\" >&2\n\
                [ -n \"$LOOM_GH_STUB_LOG\" ] && echo \"$*\" >> \"$LOOM_GH_STUB_LOG\"\n\
                exit 127\n";
            std::fs::write(&p, script).expect("write stub");
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755))
                .expect("chmod stub");
            (dir, p)
        })
        .1
        .clone()
    }

    /// RAII guard pointing `LOOM_GH_BIN` at `bin` and restoring the prior
    /// value on drop. Tests that stand up their own fake `gh` use this so the
    /// loud-failing default stub does not shadow it (#10088). Callers must
    /// serialise on the env lock, as for any other `set_var` test.
    pub(crate) struct GhBinGuard(Option<std::ffi::OsString>);

    impl GhBinGuard {
        pub(crate) fn set(bin: &std::path::Path) -> Self {
            let prior = std::env::var_os("LOOM_GH_BIN");
            std::env::set_var("LOOM_GH_BIN", bin);
            Self(prior)
        }
    }

    impl Drop for GhBinGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(v) => std::env::set_var("LOOM_GH_BIN", v),
                None => std::env::remove_var("LOOM_GH_BIN"),
            }
        }
    }
}
