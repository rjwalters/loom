//! The one place a dispatched child's admitted-launch environment is pinned
//! (Issue #8599).
//!
//! Two dispatch surfaces spawn a worker from an already-admitted
//! [`ResolvedRuntime`] — `sweep_registry::spawn_process` (the sweep child) and
//! `role_runner::launch::run_role_with_timeout` (a scheduled role tick) — and
//! both had the same hand-copied block: pin `LOOM_RUNTIME` (#4123: so
//! `spawn-worker` cannot re-resolve a different runtime after the decision was
//! made), pin `LOOM_ROLE` (#4768: so `spawn-codex.sh` sees the role it was
//! admitted for instead of silently taking its read-only sandbox fallback),
//! log the admission, and warn on a `suggestedWorkerType` divergence (#6201).
//!
//! Collapsing the two copies here is what makes room for a third pin — the
//! preference marker #8599 carries to the child — without growing either
//! dispatch surface. It is also the seam #8602 extends to pin a tap's
//! `modelProfile`, rather than re-duplicating the block a third time.
//!
//! # Why the marker travels in the environment
//!
//! The ordered-preference walk (#8436/#8554) runs in the **daemon's** process,
//! so the daemon is the only process that knows which tier was chosen. The
//! per-sweep launch record is written by the **child** (`worker_spawn::launch`,
//! beside its own `# LOOM_RUNTIME_RESOLVED` line). An environment variable is
//! the same channel the runtime and role pins already use to cross that
//! boundary, which keeps the child a pure writer of what it was told rather
//! than a second decision site.
use crate::runtime_admission::ResolvedRuntime;
use std::process::Command;

/// Carries the `# LOOM_RUNTIME_PREFERENCE …` marker line from the daemon to
/// the child that writes the launch record.
///
/// **Not** a selection input: nothing reads this to decide a runtime (that is
/// `LOOM_RUNTIME`'s job, and an operator pin disables fall-through outright).
/// It is observability payload, deliberately named apart from `LOOM_RUNTIME`
/// so it can never be mistaken for one.
pub const PREFERENCE_MARKER_ENV: &str = "LOOM_RUNTIME_PREFERENCE_MARKER";

/// Pin `cmd`'s environment to the already-admitted launch, log the admission,
/// and warn on a declared-vs-admitted runtime divergence.
///
/// `context` prefixes the log line with the dispatch surface's own name
/// (`"sweep_registry"`, `"role_runner"`), so the two surfaces stay
/// distinguishable in `loom-daemon logs` exactly as they were when each owned
/// its own copy of this block.
///
/// A `None` admission is a no-op: the caller opted out of admission (a test
/// `spawn_bin`, a hermetic dispatch fixture), so there is nothing admitted to
/// pin and the child's environment must be left exactly as it was.
pub fn apply_launch_env(cmd: &mut Command, admission: Option<&ResolvedRuntime>, context: &str) {
    let Some(admission) = admission else {
        return;
    };
    cmd.env("LOOM_RUNTIME", &admission.runtime);
    cmd.env("LOOM_ROLE", &admission.role);
    // Set ONLY when the ordered preference walk actually decided this launch.
    // On the static path the variable is never set, so a child's log stays
    // byte-identical to its pre-#8599 output (#8554's absent-config
    // invariant).
    if let Some(preference) = &admission.preference {
        cmd.env(PREFERENCE_MARKER_ENV, &preference.marker);
    }
    log::info!(
        "{context}: admitted role={} runtime={} source={}",
        admission.role,
        admission.runtime,
        admission.source
    );
    if let Some(message) =
        crate::runtime_admission::suggested_worker_type_mismatch_warning(admission)
    {
        log::warn!("{message}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::runtime_admission::RuntimeSource;
    use crate::runtime_preference::PreferenceStamp;
    use std::path::PathBuf;

    fn admitted(preference: Option<PreferenceStamp>) -> ResolvedRuntime {
        ResolvedRuntime {
            role: "sweep-lifecycle".into(),
            runtime: "opencode".into(),
            source: RuntimeSource::Preference,
            adapter: PathBuf::from("/adapter"),
            role_manifest: PathBuf::from("/role.json"),
            runtime_manifest: PathBuf::from("/runtime.json"),
            suggested_worker_type: None,
            preference,
        }
    }

    fn stamp() -> PreferenceStamp {
        PreferenceStamp {
            tier: 2,
            tap: "opencode:zai-metered".into(),
            marker: "# LOOM_RUNTIME_PREFERENCE order=claude,codex,opencode:zai-metered tier=2 \
                     tap=opencode:zai-metered source=preference"
                .into(),
        }
    }

    /// Every environment value `cmd` was told to set, as `(name, value)`.
    fn env_of(cmd: &Command) -> Vec<(String, String)> {
        cmd.get_envs()
            .filter_map(|(k, v)| {
                Some((k.to_string_lossy().into_owned(), v?.to_string_lossy().into_owned()))
            })
            .collect()
    }

    #[test]
    fn the_preference_marker_is_pinned_only_when_a_walk_decided_the_launch() {
        let mut with = Command::new("/bin/true");
        apply_launch_env(&mut with, Some(&admitted(Some(stamp()))), "test");
        assert!(env_of(&with).contains(&(PREFERENCE_MARKER_ENV.into(), stamp().marker)));

        // Absent config / operator pin / fail-closed: no marker at all, not an
        // empty one — the child must not write a preference line.
        let mut without = Command::new("/bin/true");
        apply_launch_env(&mut without, Some(&admitted(None)), "test");
        assert!(
            !env_of(&without)
                .iter()
                .any(|(name, _)| name == PREFERENCE_MARKER_ENV),
            "{:?}",
            env_of(&without)
        );
    }

    /// The runtime/role pins the two dispatch surfaces depended on before the
    /// collapse must survive it unchanged — including the no-op on `None`.
    #[test]
    fn the_runtime_and_role_pins_are_unchanged_by_the_collapse() {
        let mut cmd = Command::new("/bin/true");
        apply_launch_env(&mut cmd, Some(&admitted(None)), "test");
        let env = env_of(&cmd);
        assert!(env.contains(&("LOOM_RUNTIME".into(), "opencode".into())), "{env:?}");
        assert!(env.contains(&("LOOM_ROLE".into(), "sweep-lifecycle".into())), "{env:?}");

        let mut opted_out = Command::new("/bin/true");
        apply_launch_env(&mut opted_out, None, "test");
        assert!(env_of(&opted_out).is_empty(), "{:?}", env_of(&opted_out));
    }
}
