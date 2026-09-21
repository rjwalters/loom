//! Selection-ladder tests (issue #8401).

use super::*;
use crate::api_keys_pool::bad_marks::{active_mark, mark_bad, unmark};
use crate::api_keys_pool::classify::{classify, Classification};
use crate::api_keys_pool::registry::{add, set_enabled, ALLOWLIST_FILE};
use std::fs;

const FAKE_KEY: &str = "fake-key-must-never-be-echoed";

/// Build a *workspace* (not a pool root) whose per-repo pool holds `names`.
fn workspace_with(names: &[&str]) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    for name in names {
        add(&root, "zai", name, "ZAI_API_KEY", &format!("{FAKE_KEY}-{name}"), false).unwrap();
    }
    tmp
}

/// Pin the rotation cursor so the round-robin sequence is deterministic.
fn seed_cursor(workspace: &std::path::Path, value: &str) {
    let dir = provider_dir(&super::super::paths::per_repo_api_keys_dir(workspace), "zai");
    fs::write(dir.join(".rotation_cursor"), value).unwrap();
}

#[test]
fn consecutive_selections_alternate_between_two_accounts() {
    let tmp = workspace_with(&["alpha", "beta"]);
    seed_cursor(tmp.path(), "0");
    let mut rng = Rng::seeded(7);
    let names: Vec<String> = (0..4)
        .map(|_| {
            select_api_key(tmp.path(), "zai", Some(&mut rng))
                .unwrap()
                .name
        })
        .collect();
    assert_eq!(names, vec!["alpha", "beta", "alpha", "beta"]);
}

#[test]
fn selection_returns_the_matching_credential() {
    let tmp = workspace_with(&["alpha"]);
    let selected = select_api_key(tmp.path(), "zai", None).unwrap();
    assert_eq!(selected.name, "alpha");
    assert_eq!(selected.credential.env_name, "ZAI_API_KEY");
    assert_eq!(selected.credential.value, format!("{FAKE_KEY}-alpha"));
    let rendered = format!("{selected:?}");
    assert!(rendered.contains("<redacted>") && !rendered.contains(FAKE_KEY), "{rendered}");
}

#[test]
fn a_disabled_account_drops_out_of_selection() {
    let tmp = workspace_with(&["alpha", "beta"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    set_enabled(&root, "zai", "alpha", false).unwrap();
    seed_cursor(tmp.path(), "0");
    let mut rng = Rng::seeded(3);
    for _ in 0..3 {
        assert_eq!(
            select_api_key(tmp.path(), "zai", Some(&mut rng))
                .unwrap()
                .name,
            "beta"
        );
    }
}

#[test]
fn an_all_disabled_pool_fails_closed_with_a_diagnostic_and_no_key_material() {
    let tmp = workspace_with(&["alpha", "beta"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    for name in ["alpha", "beta"] {
        set_enabled(&root, "zai", name, false).unwrap();
    }
    let err = select_api_key(tmp.path(), "zai", None).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("All 2 API-key account(s)"), "{text}");
    assert!(text.contains("alpha: disabled by operator"), "{text}");
    assert!(text.contains("loom-daemon api-keys enable zai beta"), "{text}");
    assert!(text.contains("deciding binary:"), "{text}");
    assert!(!text.contains(FAKE_KEY), "{text}");
    assert_eq!(EX_CONFIG, 78);
}

#[test]
fn a_malformed_account_is_skipped_and_named_in_the_empty_pool_error() {
    let tmp = workspace_with(&["alpha"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    fs::write(provider_dir(&root, "zai").join("broken.env"), "no-assignment-here\n").unwrap();
    // The healthy account still wins.
    assert_eq!(select_api_key(tmp.path(), "zai", None).unwrap().name, "alpha");
    set_enabled(&root, "zai", "alpha", false).unwrap();
    let text = select_api_key(tmp.path(), "zai", None)
        .unwrap_err()
        .to_string();
    assert!(text.contains("broken: unusable file"), "{text}");
}

#[test]
fn an_unregistered_provider_names_the_add_command() {
    let tmp = workspace_with(&["alpha"]);
    let text = select_api_key(tmp.path(), "openai", None)
        .unwrap_err()
        .to_string();
    assert!(
        text.contains("No API-key accounts registered for provider \"openai\""),
        "{text}"
    );
    assert!(text.contains("loom-daemon api-keys add openai <name>"), "{text}");
}

#[test]
fn an_operator_pin_constrains_selection_but_a_stale_pin_does_not_strand_it() {
    let tmp = workspace_with(&["alpha", "beta"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    let dir = provider_dir(&root, "zai");
    fs::write(dir.join(ALLOWLIST_FILE), "# pinned\nbeta\n").unwrap();
    let mut rng = Rng::seeded(11);
    for _ in 0..3 {
        assert_eq!(
            select_api_key(tmp.path(), "zai", Some(&mut rng))
                .unwrap()
                .name,
            "beta"
        );
    }
    // A pin naming only accounts that no longer exist must fall through rather
    // than fail closed — the Claude pool's stale-advice fail-safe.
    fs::write(dir.join(ALLOWLIST_FILE), "retired-account\n").unwrap();
    let name = select_api_key(tmp.path(), "zai", Some(&mut rng))
        .unwrap()
        .name;
    assert!(name == "alpha" || name == "beta", "{name}");
}

#[test]
fn is_pooled_is_false_until_an_account_is_registered() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(!is_pooled(tmp.path(), "zai").unwrap());
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    add(&root, "zai", "alpha", "ZAI_API_KEY", FAKE_KEY, false).unwrap();
    assert!(is_pooled(tmp.path(), "zai").unwrap());
    assert!(!is_pooled(tmp.path(), "openai").unwrap());
}

#[test]
fn health_is_secret_free_and_counts_each_state() {
    let tmp = workspace_with(&["alpha", "beta"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    set_enabled(&root, "zai", "beta", false).unwrap();
    fs::write(provider_dir(&root, "zai").join("broken.env"), FAKE_KEY).unwrap();
    let snapshot = health(tmp.path(), None).unwrap();
    assert_eq!(snapshot.len(), 1);
    let zai = &snapshot[0];
    assert_eq!(zai.provider, "zai");
    assert_eq!((zai.total, zai.selectable, zai.disabled, zai.malformed), (3, 1, 1, 1));
    let json = serde_json::to_string(&snapshot).unwrap();
    assert!(!json.contains(FAKE_KEY), "{json}");
}

#[test]
fn health_on_an_empty_pool_is_empty_rather_than_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    assert!(health(tmp.path(), None).unwrap().is_empty());
    let only = health(tmp.path(), Some("zai")).unwrap();
    assert_eq!(only.len(), 1);
    assert_eq!(only[0].total, 0);
}

// =============================================================================
// Bad-marking / exhaustion (#8401 acceptance criterion: "Bad-marking a
// simulated exhausted account (classifier fixture) removes it from selection
// until its reset horizon; an all-exhausted pool exits 78 with the same
// operator diagnostic shape the token pool uses.")
// =============================================================================

#[test]
fn a_classifier_fixture_drives_a_bad_mark_that_removes_the_account_from_selection() {
    let tmp = workspace_with(&["alpha", "beta"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());

    // A *simulated* harness failure — never a real captured Z.ai response
    // (see `classify`'s module docs) — run through the real classifier.
    let simulated_output = "Error: insufficient balance for this account (code 1113)";
    let classification = classify(simulated_output, 1);
    assert_eq!(classification, Some(Classification::Exhausted));

    mark_bad(
        &root,
        "zai",
        "alpha",
        &format!("classified as {}", classification.unwrap().label()),
        Some(classification.unwrap().default_cooldown_secs()),
    )
    .unwrap();

    seed_cursor(tmp.path(), "0");
    let mut rng = Rng::seeded(1);
    for _ in 0..4 {
        assert_eq!(
            select_api_key(tmp.path(), "zai", Some(&mut rng))
                .unwrap()
                .name,
            "beta"
        );
    }
}

#[test]
fn an_all_exhausted_pool_fails_closed_at_78_with_the_token_pool_diagnostic_shape() {
    let tmp = workspace_with(&["alpha", "beta"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    for name in ["alpha", "beta"] {
        mark_bad(&root, "zai", name, "simulated exhaustion", Some(3600)).unwrap();
    }
    let err = select_api_key(tmp.path(), "zai", None).unwrap_err();
    let text = err.to_string();
    assert!(text.contains("All 2 API-key account(s)"), "{text}");
    assert!(text.contains("bad-marked"), "{text}");
    assert!(text.contains("simulated exhaustion"), "{text}");
    assert!(text.contains("api-keys unblock zai alpha"), "{text}");
    assert!(text.contains("deciding binary:"), "{text}");
    assert_eq!(EX_CONFIG, 78);
}

#[test]
fn a_bad_mark_expires_at_its_reset_horizon_and_the_account_becomes_selectable_again() {
    let tmp = workspace_with(&["alpha"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    // A 2s horizon, not 1s: the clock is whole seconds, so with a 1s horizon
    // the "still marked" `select_api_key` below failed whenever the second
    // ticked over between `mark_bad` and it (the marks write is fsynced).
    let mark = mark_bad(&root, "zai", "alpha", "simulated", Some(2)).unwrap();
    // `active_mark` takes an explicit `now`, so the "still active" and "past
    // the horizon" checks below are deterministic and do not depend on wall
    // clock timing (only the `select_api_key` checks read the real clock, via
    // `bad_marks::epoch_now`).
    assert!(active_mark(&root, "zai", "alpha", mark.marked_at + 1)
        .unwrap()
        .is_some());
    assert!(active_mark(&root, "zai", "alpha", mark.marked_at + 2)
        .unwrap()
        .is_none());
    assert!(select_api_key(tmp.path(), "zai", None).is_err());
    // Past the reset horizon: selectable again without any `unblock` call.
    std::thread::sleep(std::time::Duration::from_millis(2100));
    assert_eq!(select_api_key(tmp.path(), "zai", None).unwrap().name, "alpha");
}

#[test]
fn unblock_clears_a_mark_before_its_reset_horizon() {
    let tmp = workspace_with(&["alpha"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    mark_bad(&root, "zai", "alpha", "simulated", None).unwrap();
    assert!(select_api_key(tmp.path(), "zai", None).is_err());
    unmark(&root, "zai", "alpha").unwrap();
    assert_eq!(select_api_key(tmp.path(), "zai", None).unwrap().name, "alpha");
}

#[test]
fn health_counts_exhausted_accounts_separately_from_disabled_and_malformed() {
    let tmp = workspace_with(&["alpha", "beta", "gamma"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    set_enabled(&root, "zai", "beta", false).unwrap();
    mark_bad(&root, "zai", "gamma", "simulated", Some(3600)).unwrap();
    let snapshot = health(tmp.path(), Some("zai")).unwrap();
    let zai = &snapshot[0];
    assert_eq!((zai.total, zai.selectable, zai.disabled, zai.exhausted), (3, 1, 1, 1));
}

// =============================================================================
// Regression tests for the PR #8428 review (fail-open paths).
// =============================================================================

#[cfg(unix)]
fn chmod(path: &std::path::Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

/// Finding 1, the verdict's third table row: the same all-disabled pool, with
/// the provider directory unreadable. Before the fix `is_pooled` was `false`
/// (so the spawn launched), and `health` reported `0/0 selectable`.
#[cfg(unix)]
#[test]
fn an_unreadable_all_disabled_pool_is_an_error_not_an_absent_pool() {
    let tmp = workspace_with(&["alpha", "beta"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    for name in ["alpha", "beta"] {
        set_enabled(&root, "zai", name, false).unwrap();
    }
    let dir = provider_dir(&root, "zai");
    chmod(&dir, 0o000);
    let as_root = fs::read_dir(&dir).is_ok();
    let pooled = is_pooled(tmp.path(), "zai");
    let selected = select_api_key(tmp.path(), "zai", None);
    let listed = list_accounts(tmp.path(), None);
    let snapshot = health(tmp.path(), None);
    chmod(&dir, 0o700); // restore before asserting, so cleanup always works
    if as_root {
        return;
    }
    let err = pooled.unwrap_err();
    assert_eq!(err.kind, std::io::ErrorKind::PermissionDenied);
    assert_eq!(err.path, dir);
    let text = selected.unwrap_err().to_string();
    assert!(text.contains("cannot be read") && text.contains("PermissionDenied"), "{text}");
    assert!(!text.contains("No API-key accounts registered"), "{text}");
    assert!(listed.is_err(), "`list` must not report an unreadable pool as empty");
    let snapshot = snapshot.unwrap();
    assert_eq!(snapshot.len(), 1);
    assert!(snapshot[0]
        .unreadable
        .as_deref()
        .unwrap()
        .contains("cannot be read"));
}

/// Finding 2: with `alpha` bad-marked, a zero-length or torn marks file used
/// to turn `1/2 selectable` into `2/2 selectable`. It must withhold instead.
#[test]
fn a_torn_marks_file_never_returns_an_exhausted_account_to_selection() {
    for torn in ["", "[{\"name\":\"alpha\","] {
        let tmp = workspace_with(&["alpha", "beta"]);
        let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
        mark_bad(&root, "zai", "alpha", "simulated exhaustion", Some(3600)).unwrap();
        assert_eq!(health(tmp.path(), Some("zai")).unwrap()[0].selectable, 1);

        fs::write(provider_dir(&root, "zai").join(".bad_accounts.json"), torn).unwrap();
        let zai = &health(tmp.path(), Some("zai")).unwrap()[0];
        assert_eq!((zai.selectable, zai.unverifiable), (0, 2), "torn body {torn:?}");
        let text = select_api_key(tmp.path(), "zai", None)
            .unwrap_err()
            .to_string();
        assert!(text.contains("pool state unreadable"), "{text}");
    }
}

/// Finding 4 (second half): a key registered under the wrong variable must not
/// be injected under this profile's harness variable.
#[test]
fn an_account_assigning_a_different_variable_is_withheld_from_that_profile() {
    let tmp = workspace_with(&["alpha"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    add(&root, "zai", "stray", "OPENAI_API_KEY", "fake-openai-key", false).unwrap();
    seed_cursor(tmp.path(), "0");
    let mut rng = Rng::seeded(5);
    for _ in 0..4 {
        let selected =
            select_api_key_for(tmp.path(), "zai", Some("ZAI_API_KEY"), Some(&mut rng)).unwrap();
        assert_eq!(selected.name, "alpha");
    }
    set_enabled(&root, "zai", "alpha", false).unwrap();
    let text = select_api_key_for(tmp.path(), "zai", Some("ZAI_API_KEY"), None)
        .unwrap_err()
        .to_string();
    assert!(text.contains("assigns OPENAI_API_KEY"), "{text}");
    assert!(text.contains("--env-var ZAI_API_KEY"), "{text}");
    assert!(!text.contains("fake-openai-key"), "{text}");
}

#[cfg(unix)]
#[test]
fn an_unreadable_operator_pin_fails_closed() {
    let tmp = workspace_with(&["alpha", "beta"]);
    let root = super::super::paths::per_repo_api_keys_dir(tmp.path());
    let pin = provider_dir(&root, "zai").join(ALLOWLIST_FILE);
    fs::write(&pin, "beta\n").unwrap();
    chmod(&pin, 0o000);
    let as_root = fs::read_to_string(&pin).is_ok();
    let selected = select_api_key(tmp.path(), "zai", None);
    chmod(&pin, 0o600);
    if as_root {
        return;
    }
    assert!(selected
        .unwrap_err()
        .to_string()
        .contains("operator pin is unreadable"));
}
