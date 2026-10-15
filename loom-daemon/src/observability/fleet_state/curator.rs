//! Observed Curator role-runner cadence and last run (#11642), the triage
//! analogue of [`super::champion`]. Reads only the role runner's effective
//! resolver and process-local tick history — never a forge call, so a tick
//! pass can produce it too. Scheduling never implies the Curator curated
//! anything; `last_run_ok` is the runner's outcome, not a count of curations.
//!
//! Since #11911 Curator is a step the work finder draws and has left the role
//! runner's table, so no repo reports a role-runner Curator state: the map is
//! empty (Curator steps report through the step telemetry instead).

/// Each managed repo's observed [`CuratorState`](crate::telemetry::kinds::fleet_state::CuratorState),
/// by lowercased slug.
pub(super) fn curator_by_slug(
    roots: &[(std::path::PathBuf, String)],
) -> std::collections::BTreeMap<String, crate::telemetry::kinds::fleet_state::CuratorState> {
    use crate::role_runner;
    let ticks = role_runner::last_role_tick_snapshot();
    curator_with_ticks(roots, &ticks)
}

fn curator_with_ticks(
    roots: &[(std::path::PathBuf, String)],
    ticks: &[crate::types::RoleLastTick],
) -> std::collections::BTreeMap<String, crate::telemetry::kinds::fleet_state::CuratorState> {
    use crate::role_runner::{self, IntervalSource};
    roots
        .iter()
        .filter_map(|(root, slug)| {
            let config = role_runner::read_role_runner_config(root);
            let spec = role_runner::DEFAULT_ROLES
                .iter()
                .find(|s| s.name == "curator")?;
            let (cadence, source) =
                role_runner::resolve_interval_for_role_with_source(spec, &config);
            let tick = ticks
                .iter()
                .find(|t| t.root == *root && t.role == "curator");
            Some((
                slug.clone(),
                crate::telemetry::kinds::fleet_state::CuratorState {
                    interval_enabled: role_runner::resolve_enabled(&config)
                        && role_runner::resolve_roles(&config)
                            .iter()
                            .any(|s| s.name == "curator"),
                    cadence_secs: cadence.as_secs(),
                    cadence_source: match source {
                        IntervalSource::Env => "env",
                        IntervalSource::Config => "effective_config",
                        IntervalSource::BuiltIn => "built_in",
                    }
                    .to_string(),
                    last_run_at: tick.map(|t| t.at),
                    last_run_ok: tick.map(|t| t.ok),
                },
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #11911: Curator is a step, not a role-runner role, so no repo reports
    /// a role-runner Curator state, even with an old Curator tick on record
    /// or `curator` still named in `roles`.
    #[test]
    fn no_repo_reports_a_role_runner_curator_state() {
        let root = tempfile::tempdir().expect("root");
        std::fs::create_dir_all(root.path().join(".loom")).expect("config directory");
        std::fs::write(
            root.path().join(".loom/config.json"),
            r#"{"autonomous":{"roleRunner":{"enabled":true,"roles":["curator"],"intervalSecs":123}}}"#,
        )
        .expect("config");
        let ticks = vec![crate::types::RoleLastTick {
            root: root.path().to_path_buf(),
            role: "curator".into(),
            at: chrono::Utc::now(),
            ok: true,
            detail: None,
            consecutive_identical_failures: 0,
        }];
        let roots = vec![(root.path().to_path_buf(), "acme/repo".into())];
        assert!(curator_with_ticks(&roots, &ticks).is_empty());
    }
}
