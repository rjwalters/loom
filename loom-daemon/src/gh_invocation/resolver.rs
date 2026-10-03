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
//!
//! A caller-injected program ([`crate::gh_invocation::GhInvocation::program`],
//! for sites whose owner carries a configured `gh` such as
//! `SweepRegistryConfig.gh_bin`) bypasses the ladder entirely.

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
    resolve_from(policy_launcher_path(), std::env::var("LOOM_GH_BIN").ok())
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
