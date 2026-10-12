/// Reads only the role runner's effective resolver and process-local tick history.
pub(super) fn champion_by_slug(
    roots: &[(std::path::PathBuf, String)],
) -> std::collections::BTreeMap<String, crate::telemetry::kinds::fleet_state::ChampionState> {
    use crate::role_runner;
    let ticks = role_runner::last_role_tick_snapshot();
    champion_with_ticks(roots, &ticks)
}

fn champion_with_ticks(
    roots: &[(std::path::PathBuf, String)],
    ticks: &[crate::types::RoleLastTick],
) -> std::collections::BTreeMap<String, crate::telemetry::kinds::fleet_state::ChampionState> {
    use crate::role_runner::{self, IntervalSource};
    roots
        .iter()
        .filter_map(|(root, slug)| {
            let config = role_runner::read_role_runner_config(root);
            let spec = role_runner::DEFAULT_ROLES
                .iter()
                .find(|s| s.name == "champion")?;
            let (cadence, source) =
                role_runner::resolve_interval_for_role_with_source(spec, &config);
            let tick = ticks
                .iter()
                .find(|t| t.root == *root && t.role == "champion");
            Some((
                slug.clone(),
                crate::telemetry::kinds::fleet_state::ChampionState {
                    approval: None,
                    merge: None,
                    prompt_source: spec.prompt.to_string(),
                    prompt_hash: prompt_hash(root),
                    interval_enabled: role_runner::resolve_enabled(&config)
                        && role_runner::resolve_roles(&config)
                            .iter()
                            .any(|s| s.name == "champion"),
                    cadence_secs: cadence.as_secs(),
                    cadence_source: match source {
                        IntervalSource::Env => "env",
                        IntervalSource::Config => "effective_config",
                        IntervalSource::BuiltIn => "built_in",
                    }
                    .to_string(),
                    last_attempt_at: tick.map(|t| t.at),
                    last_attempt_ok: tick.map(|t| t.ok),
                },
            ))
        })
        .collect()
}

fn prompt_hash(root: &std::path::Path) -> Option<String> {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for name in [
        "champion.md",
        "champion-issue-promo.md",
        "champion-pr-merge.md",
    ] {
        let bytes = std::fs::read(root.join(".claude/commands/loom").join(name)).ok()?;
        digest.update(name.as_bytes());
        digest.update((bytes.len() as u64).to_be_bytes());
        digest.update(bytes);
    }
    Some(format!("{:x}", digest.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn custom_prompt_changes_provenance_without_inventing_policy() {
        let root = tempfile::tempdir().expect("tempdir");
        let commands = root.path().join(".claude/commands/loom");
        std::fs::create_dir_all(&commands).expect("commands");
        assert_eq!(prompt_hash(root.path()), None);
        for name in [
            "champion.md",
            "champion-issue-promo.md",
            "champion-pr-merge.md",
        ] {
            std::fs::write(commands.join(name), "custom operator-only policy").expect("prompt");
        }
        let old = prompt_hash(root.path()).expect("hash");
        std::fs::write(commands.join("champion.md"), "different policy").expect("prompt");
        assert_ne!(Some(old), prompt_hash(root.path()));
    }
    #[test]
    fn attempts_are_attributed_to_exact_root_and_failed_skips_are_not_success() {
        let first = tempfile::tempdir().expect("first");
        let second = tempfile::tempdir().expect("second");
        let at = chrono::Utc::now();
        let ticks = vec![crate::types::RoleLastTick {
            root: first.path().to_path_buf(),
            role: "champion".into(),
            at,
            ok: false,
            detail: Some("NoTokenPool".into()),
            consecutive_identical_failures: 2,
        }];
        let roots = vec![
            (first.path().to_path_buf(), "acme/first".into()),
            (second.path().to_path_buf(), "acme/second".into()),
        ];
        let state = champion_with_ticks(&roots, &ticks);
        assert_eq!(state["acme/first"].last_attempt_at, Some(at));
        assert_eq!(state["acme/first"].last_attempt_ok, Some(false));
        assert_eq!(state["acme/second"].last_attempt_at, None);
        assert_eq!(state["acme/second"].last_attempt_ok, None);
        assert_eq!(state["acme/first"].approval, None);
        assert_eq!(state["acme/second"].merge, None);
    }
}
