//! Unit tests for [`super`] — the `branch-update strategy` policy (#10771).

use super::*;
use serde_json::json;

fn settings(merge: Option<bool>, squash: Option<bool>, rebase: Option<bool>) -> Probe {
    Probe::Known(MergeSettings {
        allow_merge_commit: merge,
        allow_squash_merge: squash,
        allow_rebase_merge: rebase,
    })
}

fn auto_with(probe: Probe) -> Strategy {
    decide(None, None, || probe).strategy
}

/// The full `auto` matrix over the three forge flags, plus every unknown
/// shape. Rebase only when `allow_rebase_merge` is positively `true`.
#[test]
fn auto_rebases_only_when_the_forge_allows_rebase_merges() {
    let b = [Some(true), Some(false), None];
    for merge in b {
        for squash in b {
            for rebase in b {
                let want = if rebase == Some(true) {
                    Strategy::Rebase
                } else {
                    Strategy::Merge
                };
                assert_eq!(
                    auto_with(settings(merge, squash, rebase)),
                    want,
                    "merge={merge:?} squash={squash:?} rebase={rebase:?}"
                );
            }
        }
    }
}

/// The live #10771 repositories: merge-commit only.
#[test]
fn a_merge_commit_only_repo_merges() {
    let d = decide(None, None, || settings(Some(true), Some(false), Some(false)));
    assert_eq!(d.strategy, Strategy::Merge);
    assert!(d.reason.contains("allow_rebase_merge=false"), "{}", d.reason);
}

#[test]
fn unknown_settings_fail_toward_merge() {
    let d = decide(None, None, || Probe::Unknown("HTTP 403".to_string()));
    assert_eq!(d.strategy, Strategy::Merge);
    assert!(d.reason.contains("HTTP 403") && d.reason.contains("failing toward merge"));
    let hidden = decide(None, None, || settings(None, None, None));
    assert_eq!(hidden.strategy, Strategy::Merge);
    assert!(hidden.reason.contains("not visible"));
}

#[test]
fn env_beats_config_beats_forge() {
    let cfg = json!("rebase");
    let never = || -> Probe { panic!("an explicit override must not probe the forge") };
    assert_eq!(decide(Some("merge"), Some(&cfg), never).strategy, Strategy::Merge);
    assert_eq!(decide(None, Some(&cfg), never).strategy, Strategy::Rebase);
    let cfg_merge = json!("merge");
    let d = decide(None, Some(&cfg_merge), never);
    assert_eq!(d.strategy, Strategy::Merge);
    assert_eq!(d.reason, format!("{CONFIG_KEY}=merge"));
    // Overrides beat the forge in both directions.
    assert_eq!(
        decide(Some("rebase"), None, never).strategy,
        Strategy::Rebase,
        "an operator may opt a rebase-barred repo into rebase explicitly"
    );
}

#[test]
fn auto_override_and_empty_env_defer_to_the_forge() {
    let cfg = json!("auto");
    assert_eq!(
        decide(None, Some(&cfg), || settings(None, None, Some(true))).strategy,
        Strategy::Rebase
    );
    assert_eq!(
        decide(Some("AUTO"), Some(&json!("merge")), || settings(None, None, Some(true))).strategy,
        Strategy::Rebase,
        "an explicit env auto wins over a config value"
    );
    assert_eq!(
        decide(Some("  "), Some(&json!("rebase")), || unreachable!()).strategy,
        Strategy::Rebase,
        "an empty env var is unset, so the config answers"
    );
    assert_eq!(
        decide(None, Some(&Value::Null), || settings(None, None, Some(false))).strategy,
        Strategy::Merge
    );
}

#[test]
fn an_unreadable_override_fails_toward_merge_without_probing() {
    let never = || -> Probe { panic!("must not probe") };
    let d = decide(Some("squash"), None, never);
    assert_eq!(d.strategy, Strategy::Merge);
    assert!(d.reason.contains("'squash'"), "{}", d.reason);
    assert_eq!(decide(None, Some(&json!("Rebase!")), never).strategy, Strategy::Merge);
    assert_eq!(decide(None, Some(&json!(true)), never).strategy, Strategy::Merge);
    // Case and whitespace are tolerated for the three real words.
    assert_eq!(decide(Some(" Rebase "), None, never).strategy, Strategy::Rebase);
}

#[test]
fn the_cache_is_per_repo_and_expires() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let path = tmp.path().join(CACHE_FILE);
    let s = MergeSettings {
        allow_merge_commit: Some(true),
        allow_squash_merge: Some(false),
        allow_rebase_merge: Some(false),
    };
    assert_eq!(read_cache(&path, "o/r", 1000, CACHE_TTL), None, "no file yet");
    write_cache(&path, "o/r", 1000, s);
    assert_eq!(read_cache(&path, "o/r", 1000 + 10, CACHE_TTL), Some(s));
    assert_eq!(read_cache(&path, "o/other", 1010, CACHE_TTL), None, "keyed by repo");
    let ttl = CACHE_TTL.as_secs();
    assert_eq!(read_cache(&path, "o/r", 1000 + ttl, CACHE_TTL), None, "expired");
    assert_eq!(read_cache(&path, "o/r", 999, CACHE_TTL), None, "from the future");
    std::fs::write(&path, "not json").expect("write");
    assert_eq!(read_cache(&path, "o/r", 1000, CACHE_TTL), None, "corrupt = miss");
}

#[test]
fn settings_decode_from_a_repos_response() {
    let body = r#"{"id":1,"allow_merge_commit":true,"allow_rebase_merge":false}"#;
    let s: MergeSettings = serde_json::from_str(body).expect("decode");
    assert_eq!(s.allow_merge_commit, Some(true));
    assert_eq!(s.allow_rebase_merge, Some(false));
    assert_eq!(s.allow_squash_merge, None, "an absent flag is unknown, not false");
}
