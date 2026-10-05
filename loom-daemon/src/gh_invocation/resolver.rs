//! The single `gh` executable resolver (#9985 slice 1).
//!
//! Precedence, first hit wins:
//!
//! 1. `toolchain.launcherPath` from the resolved forge-egress policy (C1,
//!    #9983; wired by #9995). Only an **env**- or **machine**-origin policy may
//!    choose the executable — a repo-local policy never can (the same trust
//!    rule as the negative canary, [`may_choose_executable`](crate::forge_egress::policy::Origin::may_choose_executable)) — and
//!    only when the launcher exists on disk. Anything else (unconfigured,
//!    unreadable, repo-origin, no `launcherPath`, launcher missing) falls
//!    through to the next rung. `LOOM_GH_NO_POLICY_LAUNCHER=1`
//!    ([`NO_POLICY_LAUNCHER_ENV`]) declines this rung outright — the seam every
//!    test harness that stubs `gh` sets, so a host's policy launcher can never
//!    outrank the stub.
//! 2. `LOOM_GH_BIN` — the test/override hook every existing resolver honours.
//!    Read with `std::env::var` semantics, byte-identical to the ten
//!    hand-rolled `gh_bin*` resolver copies this replaces (a set-but-empty value is
//!    returned as-is, a non-UTF-8 value is treated as unset).
//! 3. Bare `"gh"`, resolved from `PATH` by the OS at spawn time.
//!
//! The policy is resolved per call, uncached. On a host with no policy that is
//! one env read and one `stat` of the machine path (`NotFound`); with one it
//! adds a read + parse of a small JSON file — microseconds against the
//! milliseconds of the `gh` spawn it precedes. Not caching also means a policy
//! change (e.g. C4 installing the launcher) takes effect on the next spawn
//! without a daemon restart.

use std::path::Path;

use crate::forge_egress::policy::{self, PolicySources, Resolution};

/// Set to `1` to decline the policy-launcher rung, so `LOOM_GH_BIN` (else bare
/// `gh` on `PATH`) wins even when an env/machine forge-egress policy names an
/// existing `toolchain.launcherPath`.
///
/// For test harnesses that hand the daemon a fake `gh` (`LOOM_GH_BIN` or a
/// stub on `PATH`): without it, a host carrying an egress policy would exec the
/// real managed `gh` instead of the stub (the #10088 hazard, out of process).
/// It is not a silent policy bypass: env is already the most-trusted policy
/// origin (`LOOM_FORGE_EGRESS_POLICY` outranks the machine policy wholesale),
/// and the forge-egress validator reports where a declined rung lands. The
/// version floor reads the exec target; a landing on bare `gh` is what
/// `toolchain.launcher-not-first` measures; a landing on `LOOM_GH_BIN` (or any
/// exec target that is neither the existing launcher nor `PATH`'s `gh`) raises
/// `toolchain.policy-launcher-declined`. All three are routing findings, so
/// under `enforcement.api = required` `assert` fails. The report's
/// `observed.ghSource` names the rung that won.
pub const NO_POLICY_LAUNCHER_ENV: &str = "LOOM_GH_NO_POLICY_LAUNCHER";

/// Whether `value` (of [`NO_POLICY_LAUNCHER_ENV`]) declines the policy rung.
/// Only the exact value `1` does; unset, empty, `0` or anything else leaves the
/// rung on, so a typo fails toward the policy.
#[must_use]
pub fn policy_rung_declined(value: Option<&std::ffi::OsStr>) -> bool {
    value.is_some_and(|v| v == "1")
}

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

impl GhBinSource {
    /// Stable lowercase name (telemetry `launcher` attribute, egress report
    /// `observed.ghSource`).
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Policy => "policy",
            Self::EnvOverride => "env_override",
            Self::Path => "path",
            Self::Injected => "injected",
        }
    }
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
/// (`forge_cmd::gh_bin` re-exports it).
#[must_use]
pub fn gh_bin() -> String {
    resolve().program
}

/// `toolchain.launcherPath` from the live process's forge-egress policy.
///
/// Sources come from [`PolicySources::from_process`] with **no repo root**, so
/// the repo tier is never consulted; [`launcher_from_sources`] additionally
/// refuses a repo-origin document, so the rule holds even for injected sources.
///
/// In a unit-test build this rung is off: a machine or env policy on the
/// test host naming a real launcher would otherwise outrank the loud-failing
/// stub [`env_override`] supplies (#10088) and let `cargo test` reach the real
/// `gh`. The rung's logic is covered through [`launcher_from_sources`].
/// Out-of-process harnesses (integration tests, shell suites) have no
/// `cfg(test)`; they set [`NO_POLICY_LAUNCHER_ENV`] instead.
fn policy_launcher_path() -> Option<String> {
    if cfg!(test) || policy_rung_declined(std::env::var_os(NO_POLICY_LAUNCHER_ENV).as_deref()) {
        return None;
    }
    launcher_from_sources(&PolicySources::from_process(None))
}

/// The policy rung, pure over injected `sources` (tests never touch `/etc` or
/// the process environment).
///
/// `Some(launcherPath)` only when the winning policy loaded, its origin
/// [`may_choose_executable`](crate::forge_egress::policy::Origin::may_choose_executable),
/// `toolchain.launcherPath` is a non-empty string, and that path exists.
/// Every other case is `None` (fall through to `LOOM_GH_BIN` / `PATH`).
#[must_use]
pub fn launcher_from_sources(sources: &PolicySources) -> Option<String> {
    let Resolution::Loaded(doc) = policy::resolve(sources) else {
        return None;
    };
    if !doc.origin.may_choose_executable() {
        return None;
    }
    let launcher = policy::dig_str(&doc.data, &["toolchain", "launcherPath"]);
    (!launcher.is_empty() && Path::new(launcher).exists()).then(|| launcher.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A temp dir holding a fake launcher and a policy naming it.
    struct Fixture {
        dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("launcher"), "#!/bin/sh\n").unwrap();
            Self { dir }
        }

        fn launcher(&self) -> PathBuf {
            self.dir.path().join("launcher")
        }

        /// Write a policy whose `launcherPath` is `launcher`; return its path.
        fn policy(&self, name: &str, launcher: &Path) -> PathBuf {
            let path = self.dir.path().join(name);
            let doc = serde_json::json!({
                "schemaVersion": 1,
                "toolchain": { "launcherPath": launcher.display().to_string() },
            });
            std::fs::write(&path, doc.to_string()).unwrap();
            path
        }
    }

    /// The ladder as `resolve()` runs it, with injected policy sources.
    fn ladder(sources: &PolicySources, env_override: Option<&str>) -> ResolvedGh {
        resolve_from(launcher_from_sources(sources), env_override.map(str::to_string))
    }

    #[test]
    fn env_origin_policy_with_existing_launcher_wins() {
        let f = Fixture::new();
        let sources = PolicySources {
            env_path: Some(f.policy("env.json", &f.launcher())),
            ..PolicySources::default()
        };
        let r = ladder(&sources, Some("/stub/gh"));
        assert_eq!(r.source, GhBinSource::Policy);
        assert_eq!(r.program, f.launcher().display().to_string());
    }

    #[test]
    fn machine_origin_policy_with_existing_launcher_wins() {
        let f = Fixture::new();
        let sources = PolicySources {
            machine_path: Some(f.policy("machine.json", &f.launcher())),
            ..PolicySources::default()
        };
        let r = ladder(&sources, None);
        assert_eq!(r.source, GhBinSource::Policy);
        assert_eq!(r.program, f.launcher().display().to_string());
    }

    #[test]
    fn repo_origin_policy_never_chooses_the_executable() {
        let f = Fixture::new();
        let sources = PolicySources {
            repo_path: Some(f.policy("repo.json", &f.launcher())),
            ..PolicySources::default()
        };
        assert_eq!(launcher_from_sources(&sources), None);
        let r = ladder(&sources, Some("/stub/gh"));
        assert_eq!((r.program.as_str(), r.source), ("/stub/gh", GhBinSource::EnvOverride));
        let r = ladder(&sources, None);
        assert_eq!((r.program.as_str(), r.source), ("gh", GhBinSource::Path));
    }

    #[test]
    fn missing_launcher_falls_through() {
        let f = Fixture::new();
        let sources = PolicySources {
            machine_path: Some(f.policy("machine.json", &f.dir.path().join("absent"))),
            ..PolicySources::default()
        };
        let r = ladder(&sources, Some("/stub/gh"));
        assert_eq!((r.program.as_str(), r.source), ("/stub/gh", GhBinSource::EnvOverride));
    }

    #[test]
    fn empty_or_absent_launcher_path_falls_through() {
        let f = Fixture::new();
        let empty = f.policy("empty.json", Path::new(""));
        let none = f.dir.path().join("none.json");
        std::fs::write(&none, r#"{"schemaVersion": 1}"#).unwrap();
        for p in [empty, none] {
            let sources = PolicySources {
                env_path: Some(p),
                ..PolicySources::default()
            };
            assert_eq!(ladder(&sources, None).source, GhBinSource::Path);
        }
    }

    #[test]
    fn unreadable_or_unconfigured_policy_falls_through() {
        let f = Fixture::new();
        // An explicitly named but missing env policy resolves `Unreadable`.
        let missing = PolicySources {
            env_path: Some(f.dir.path().join("missing.json")),
            ..PolicySources::default()
        };
        assert_eq!(ladder(&missing, Some("/stub/gh")).source, GhBinSource::EnvOverride);
        // Unparseable.
        let garbage = f.dir.path().join("garbage.json");
        std::fs::write(&garbage, "{not json").unwrap();
        let unparseable = PolicySources {
            env_path: Some(garbage),
            ..PolicySources::default()
        };
        assert_eq!(ladder(&unparseable, None).source, GhBinSource::Path);
        // No candidate at all.
        assert_eq!(ladder(&PolicySources::default(), None).source, GhBinSource::Path);
    }

    #[test]
    fn only_the_exact_value_1_declines_the_policy_rung() {
        use std::ffi::OsStr;
        assert!(policy_rung_declined(Some(OsStr::new("1"))));
        for v in ["", "0", "true", "yes", " 1", "11"] {
            assert!(!policy_rung_declined(Some(OsStr::new(v))), "{v:?}");
        }
        assert!(!policy_rung_declined(None));
    }

    #[test]
    fn env_policy_outranks_machine_policy_for_the_launcher() {
        let f = Fixture::new();
        let other = f.dir.path().join("other-launcher");
        std::fs::write(&other, "#!/bin/sh\n").unwrap();
        let sources = PolicySources {
            env_path: Some(f.policy("env.json", &other)),
            machine_path: Some(f.policy("machine.json", &f.launcher())),
            repo_path: None,
            ..PolicySources::default()
        };
        assert_eq!(launcher_from_sources(&sources), Some(other.display().to_string()));
    }
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
            // Named prefix so an exec trace can tell this stub apart from a
            // test's own fake `gh` (#10138).
            let dir = tempfile::Builder::new()
                .prefix("loom-gh-test-stub-")
                .tempdir()
                .expect("stub tempdir");
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
