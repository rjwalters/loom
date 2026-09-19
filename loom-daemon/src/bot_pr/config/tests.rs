use super::*;
use serde_json::json;

#[test]
fn absent_block_is_off_with_dependabot_trusted_and_no_semver_ceiling() {
    let cfg = from_value(&json!({"terminals": []}));
    assert_eq!(cfg, BotPrConfig::default());
    assert!(!cfg.enabled, "AC6: flag off => behavior identical to today");
    assert!(cfg.trusts("dependabot[bot]"));
    assert_eq!(cfg.max_semver, MaxSemver::All);
}

#[test]
fn empty_document_is_off() {
    assert!(!from_value(&json!({})).enabled);
    assert!(!from_value(&json!(null)).enabled);
    assert!(!from_value(&json!("not an object")).enabled);
}

#[test]
fn camel_case_keys_are_canonical() {
    let cfg = from_value(&json!({
        "champion": {
            "autoMergeDependabot": true,
            "trustedBotAuthors": ["dependabot[bot]", "renovate[bot]"],
            "dependabotMaxSemver": "minor"
        }
    }));
    assert!(cfg.enabled);
    assert_eq!(cfg.trusted_bot_authors, ["dependabot[bot]", "renovate[bot]"]);
    assert_eq!(cfg.max_semver, MaxSemver::Minor);
}

#[test]
fn snake_case_keys_from_the_issue_body_are_accepted_as_aliases() {
    // Issue #4765 wrote the proposal in snake_case. A config copy-pasted from
    // it must enable the feature, not be silently ignored.
    let cfg = from_value(&json!({
        "champion": {
            "auto_merge_dependabot": true,
            "trusted_bot_authors": ["dependabot[bot]"],
            "dependabot_max_semver": "patch"
        }
    }));
    assert!(cfg.enabled);
    assert_eq!(cfg.max_semver, MaxSemver::Patch);
}

#[test]
fn camel_case_wins_when_both_spellings_are_present() {
    let cfg = from_value(&json!({
        "champion": { "autoMergeDependabot": true, "auto_merge_dependabot": false }
    }));
    assert!(cfg.enabled);
}

#[test]
fn unrecognized_semver_value_falls_back_to_all_rather_than_erroring() {
    let cfg = from_value(&json!({
        "champion": { "autoMergeDependabot": true, "dependabotMaxSemver": "banana" }
    }));
    assert_eq!(cfg.max_semver, MaxSemver::All);
}

#[test]
fn wrong_typed_fields_fall_through_to_defaults() {
    let cfg = from_value(&json!({
        "champion": {
            "autoMergeDependabot": "yes",
            "trustedBotAuthors": "dependabot[bot]",
            "dependabotMaxSemver": 3
        }
    }));
    assert!(!cfg.enabled);
    assert_eq!(cfg.trusted_bot_authors, [DEFAULT_TRUSTED_BOT_AUTHOR]);
    assert_eq!(cfg.max_semver, MaxSemver::All);
}

#[test]
fn an_explicitly_empty_trusted_list_trusts_nobody() {
    let cfg = from_value(&json!({
        "champion": { "autoMergeDependabot": true, "trustedBotAuthors": [] }
    }));
    assert!(cfg.trusted_bot_authors.is_empty());
    assert!(!cfg.trusts("dependabot[bot]"));
}

#[test]
fn author_match_is_exact_not_substring() {
    let cfg = from_value(&json!({
        "champion": { "autoMergeDependabot": true, "trustedBotAuthors": ["dependabot[bot]"] }
    }));
    assert!(cfg.trusts("dependabot[bot]"));
    assert!(cfg.trusts("Dependabot[bot]"), "logins are case-insensitive");
    assert!(cfg.trusts("  dependabot[bot] "), "surrounding space tolerated");
    // The spoof shapes: a human account whose name merely contains the bot's.
    assert!(!cfg.trusts("dependabot"));
    assert!(!cfg.trusts("not-dependabot[bot]"));
    assert!(!cfg.trusts("dependabot[bot]-impostor"));
    assert!(!cfg.trusts(""));
}

#[test]
fn max_semver_parses_the_documented_spellings() {
    assert_eq!(MaxSemver::parse("patch"), Some(MaxSemver::Patch));
    assert_eq!(MaxSemver::parse("MINOR"), Some(MaxSemver::Minor));
    assert_eq!(MaxSemver::parse(" all "), Some(MaxSemver::All));
    assert_eq!(MaxSemver::parse("nope"), None);
}
