use super::*;
use crate::bot_pr::classify::{Disqualified, Qualified, WAIVED_CRITERIA};
use crate::bot_pr::config::{BotPrConfig, MaxSemver};
use crate::bot_pr::semver_guard::BumpLevel;

#[test]
fn single_quotes_in_an_untrusted_value_cannot_escape_the_quoting() {
    // A PR title is untrusted external content. This is the shape that would
    // break out of a naive `KEY='…'` render.
    assert_eq!(sh_quote("it's"), r"'it'\''s'");
    assert_eq!(sh_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    assert_eq!(sh_quote("`id`"), "'`id`'");
    assert_eq!(sh_quote(""), "''");
}

#[test]
fn config_render_is_evalable_and_names_every_knob() {
    let out = render_config(&BotPrConfig {
        enabled: true,
        trusted_bot_authors: vec!["dependabot[bot]".into(), "renovate[bot]".into()],
        max_semver: MaxSemver::Minor,
    });
    assert!(out.contains("BOT_PR_ENABLED='true'\n"));
    assert!(out.contains("BOT_PR_TRUSTED_AUTHORS='dependabot[bot],renovate[bot]'\n"));
    assert!(out.contains("BOT_PR_MAX_SEMVER='minor'\n"));
}

#[test]
fn the_default_config_renders_disabled() {
    assert!(render_config(&BotPrConfig::default()).contains("BOT_PR_ENABLED='false'\n"));
}

#[test]
fn a_qualifying_verdict_names_exactly_the_two_waived_criteria() {
    let out = render_verdict(&Ok(Qualified {
        bump_level: Some(BumpLevel::Patch),
        waived: WAIVED_CRITERIA.to_vec(),
    }));
    assert!(out.contains("BOT_PR_QUALIFIES='true'\n"));
    assert!(out.contains("BOT_PR_REASON='qualifies'\n"));
    assert!(out.contains("BOT_PR_WAIVED='critical-file,recency-doctor-route'\n"));
    assert!(out.contains("BOT_PR_BUMP_LEVEL='patch'\n"));
    assert!(out.contains("BOT_PR_OFFENDING_FILE=''\n"));
}

#[test]
fn a_disqualifying_verdict_carries_the_offending_file() {
    let out = render_verdict(&Err(Disqualified::NonManifestFile {
        path: "src/main.rs".into(),
    }));
    assert!(out.contains("BOT_PR_QUALIFIES='false'\n"));
    assert!(out.contains("BOT_PR_REASON='non-manifest-file'\n"));
    assert!(out.contains("BOT_PR_OFFENDING_FILE='src/main.rs'\n"));
    assert!(out.contains("BOT_PR_WAIVED=''\n"));
}

#[test]
fn every_verdict_emits_the_same_six_keys() {
    let verdicts = [
        Ok(Qualified {
            bump_level: None,
            waived: WAIVED_CRITERIA.to_vec(),
        }),
        Err(Disqualified::Disabled),
        Err(Disqualified::UntrustedAuthor {
            author: "someone".into(),
        }),
        Err(Disqualified::SemverExceeded {
            level: BumpLevel::Major,
            max: "minor",
        }),
    ];
    for v in &verdicts {
        let out = render_verdict(v);
        for key in [
            "BOT_PR_QUALIFIES=",
            "BOT_PR_REASON=",
            "BOT_PR_DETAIL=",
            "BOT_PR_WAIVED=",
            "BOT_PR_BUMP_LEVEL=",
            "BOT_PR_OFFENDING_FILE=",
        ] {
            assert!(out.contains(key), "{key} missing from {out}");
        }
    }
}
