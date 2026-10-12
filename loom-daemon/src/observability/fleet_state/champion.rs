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
                    standing_auth_ids: standing_auth_ids(root),
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

/// Valid `champion.standingAuthorizations` class ids, read from the same
/// `.loom/config.json` the installed prompt's script reads, mirroring its
/// fail-safe entry validation: no id, no usable `filePatterns` list, or any
/// condition outside the closed vocabulary authorizes nothing.
fn standing_auth_ids(root: &std::path::Path) -> Vec<String> {
    use serde_json::Value;
    let Ok(bytes) = std::fs::read(root.join(".loom/config.json")) else {
        return Vec::new();
    };
    let Ok(config) = serde_json::from_slice::<Value>(&bytes) else {
        return Vec::new();
    };
    let Some(classes) = config
        .get("champion")
        .and_then(|champion| champion.get("standingAuthorizations"))
        .and_then(|value| value.as_array())
    else {
        return Vec::new();
    };
    let mut ids: Vec<String> = classes
        .iter()
        .filter(|entry| {
            let id_ok = entry
                .get("id")
                .and_then(|value| value.as_str())
                .is_some_and(|id| !id.is_empty());
            let patterns_ok = entry
                .get("filePatterns")
                .and_then(|value| value.as_array())
                .is_some_and(|patterns| {
                    patterns
                        .iter()
                        .any(|pattern| pattern.as_str().is_some_and(|s| !s.is_empty()))
                });
            let conditions_ok = entry
                .get("conditions")
                .and_then(|value| value.as_array())
                .is_some_and(|conditions| {
                    !conditions.is_empty()
                        && conditions.iter().all(|condition| {
                            matches!(condition.as_str(), Some("judgeApproval") | Some("greenCi"))
                        })
                });
            id_ok && patterns_ok && conditions_ok
        })
        .filter_map(|entry| entry.get("id").and_then(|value| value.as_str()))
        .map(str::to_string)
        .collect();
    ids.sort();
    ids.dedup();
    ids
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
    Some(
        digest
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
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
        for name in [
            "champion.md",
            "champion-issue-promo.md",
            "champion-pr-merge.md",
        ] {
            let old = prompt_hash(root.path()).expect("hash");
            std::fs::write(commands.join(name), format!("changed {name}")).expect("prompt");
            assert_ne!(Some(old), prompt_hash(root.path()));
        }
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
    #[test]
    fn effective_config_selects_champion_and_cadence() {
        let root = tempfile::tempdir().expect("root");
        std::fs::create_dir_all(root.path().join(".loom")).expect("config directory");
        std::fs::write(root.path().join(".loom/config.json"), r#"{"autonomous":{"roleRunner":{"enabled":true,"roles":["champion"],"intervalSecs":123}}}"#).expect("config");
        let roots = vec![(root.path().to_path_buf(), "acme/repo".into())];
        let actual = champion_with_ticks(&roots, &[]);
        let config = crate::role_runner::read_role_runner_config(root.path());
        let spec = crate::role_runner::resolve_roles(&config)
            .into_iter()
            .find(|s| s.name == "champion")
            .expect("Champion selected");
        assert_eq!(actual["acme/repo"].prompt_source, spec.prompt);
        assert_eq!(
            actual["acme/repo"].cadence_secs,
            crate::role_runner::resolve_interval_for_role(&spec, &config).as_secs()
        );
        assert!(actual["acme/repo"].interval_enabled);
        assert_eq!(actual["acme/repo"].last_attempt_at, None);
    }
    #[test]
    fn standing_authorizations_pin_fast_path_repos_against_judgment_repos() {
        let auto = tempfile::tempdir().expect("auto root");
        std::fs::create_dir_all(auto.path().join(".loom")).expect("config directory");
        std::fs::write(
            auto.path().join(".loom/config.json"),
            r#"{"champion":{"standingAuthorizations":[
                {"id":"docs-only","filePatterns":["README.md"],"conditions":["judgeApproval"]},
                {"id":"guard-hooks","filePatterns":[".loom/hooks/guard-*.sh"],"conditions":["judgeApproval","greenCi"]}
            ]}}"#,
        )
        .expect("config");
        let judgment = tempfile::tempdir().expect("judgment root");
        std::fs::create_dir_all(judgment.path().join(".loom")).expect("config directory");
        std::fs::write(judgment.path().join(".loom/config.json"), "{}").expect("config");
        let roots = vec![
            (auto.path().to_path_buf(), "acme/auto".into()),
            (judgment.path().to_path_buf(), "acme/judgment".into()),
        ];
        let state = champion_with_ticks(&roots, &[]);
        assert_eq!(
            state["acme/auto"].standing_auth_ids,
            vec!["docs-only".to_string(), "guard-hooks".to_string()]
        );
        assert!(state["acme/judgment"].standing_auth_ids.is_empty());
    }
    #[test]
    fn malformed_standing_authorization_entries_authorize_nothing() {
        let root = tempfile::tempdir().expect("root");
        std::fs::create_dir_all(root.path().join(".loom")).expect("config directory");
        std::fs::write(
            root.path().join(".loom/config.json"),
            r#"{"champion":{"standingAuthorizations":[
                {"filePatterns":["a.sh"],"conditions":["judgeApproval"]},
                {"id":"no-patterns","conditions":["judgeApproval"]},
                {"id":"empty-patterns","filePatterns":[],"conditions":["judgeApproval"]},
                {"id":"blank-patterns","filePatterns":["",""],"conditions":["judgeApproval"]},
                {"id":"patterns-not-array","filePatterns":"a.sh","conditions":["judgeApproval"]},
                {"id":"no-conditions","filePatterns":["a.sh"]},
                {"id":"empty-conditions","filePatterns":["a.sh"],"conditions":[]},
                {"id":"unknown-condition","filePatterns":["a.sh"],"conditions":["judgeApproval","reviewedByTwoHumans"]},
                {"id":"valid","filePatterns":["a.sh"],"conditions":["greenCi"]}
            ]}}"#,
        )
        .expect("config");
        let roots = vec![(root.path().to_path_buf(), "acme/repo".into())];
        let state = champion_with_ticks(&roots, &[]);
        assert_eq!(state["acme/repo"].standing_auth_ids, vec!["valid".to_string()]);
    }
    #[test]
    fn missing_or_unreadable_config_declares_no_standing_authorization() {
        let missing = tempfile::tempdir().expect("missing");
        let broken = tempfile::tempdir().expect("broken");
        std::fs::create_dir_all(broken.path().join(".loom")).expect("config directory");
        std::fs::write(broken.path().join(".loom/config.json"), "{not json").expect("config");
        let roots = vec![
            (missing.path().to_path_buf(), "acme/missing".into()),
            (broken.path().to_path_buf(), "acme/broken".into()),
        ];
        let state = champion_with_ticks(&roots, &[]);
        assert!(state["acme/missing"].standing_auth_ids.is_empty());
        assert!(state["acme/broken"].standing_auth_ids.is_empty());
    }
}
