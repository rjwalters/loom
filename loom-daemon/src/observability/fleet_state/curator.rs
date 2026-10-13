//! Observed Curator role-runner cadence and last run (#11642), the triage
//! analogue of [`super::champion`]. Reads only the role runner's effective
//! resolver and process-local tick history — never a forge call, so a tick
//! pass can produce it too. Scheduling never implies the Curator curated
//! anything; `last_run_ok` is the runner's outcome, not a count of curations.

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
    #[test]
    fn attempts_are_attributed_to_exact_root_and_failed_skips_are_not_success() {
        let first = tempfile::tempdir().expect("first");
        let second = tempfile::tempdir().expect("second");
        let at = chrono::Utc::now();
        let ticks = vec![crate::types::RoleLastTick {
            root: first.path().to_path_buf(),
            role: "curator".into(),
            at,
            ok: false,
            detail: Some("NoTokenPool".into()),
            consecutive_identical_failures: 2,
        }];
        let roots = vec![
            (first.path().to_path_buf(), "acme/first".into()),
            (second.path().to_path_buf(), "acme/second".into()),
        ];
        let state = curator_with_ticks(&roots, &ticks);
        assert_eq!(state["acme/first"].last_run_at, Some(at));
        assert_eq!(state["acme/first"].last_run_ok, Some(false));
        // A root with no recorded tick reports neither — never a guessed run.
        assert_eq!(state["acme/second"].last_run_at, None);
        assert_eq!(state["acme/second"].last_run_ok, None);
    }
    #[test]
    fn effective_config_selects_curator_and_cadence() {
        let root = tempfile::tempdir().expect("root");
        std::fs::create_dir_all(root.path().join(".loom")).expect("config directory");
        std::fs::write(
            root.path().join(".loom/config.json"),
            r#"{"autonomous":{"roleRunner":{"enabled":true,"roles":["curator"],"intervalSecs":123}}}"#,
        )
        .expect("config");
        let roots = vec![(root.path().to_path_buf(), "acme/repo".into())];
        let actual = curator_with_ticks(&roots, &[]);
        let config = crate::role_runner::read_role_runner_config(root.path());
        let spec = crate::role_runner::resolve_roles(&config)
            .into_iter()
            .find(|s| s.name == "curator")
            .expect("Curator selected");
        assert_eq!(
            actual["acme/repo"].cadence_secs,
            crate::role_runner::resolve_interval_for_role(&spec, &config).as_secs()
        );
        assert_eq!(actual["acme/repo"].cadence_secs, 123);
        assert!(actual["acme/repo"].interval_enabled);
        assert_eq!(actual["acme/repo"].last_run_at, None);
    }
}
