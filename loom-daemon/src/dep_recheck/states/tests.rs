//! Tests for `dep-states` (#14710). The forge-touching half
//! (`forge::fetch_bound_state`) talks to a `gh` stub only through the shell
//! suite (`test-dep-recheck-fingerprint.sh`'s dep-states section), the same
//! split every sibling module here uses; what is unit-tested is the parsing,
//! the binding check, and the state mapping — the pure functions a wrong
//! answer would have to slip through.

use super::*;

fn spec(repo: Option<&str>, number: i64) -> ParsedRef {
    ParsedRef::Spec(RefSpec {
        repo: repo.map(str::to_string),
        number,
    })
}

#[test]
fn bare_numbers_parse_without_a_repo() {
    assert_eq!(parse_token("200"), spec(None, 200));
    assert_eq!(parse_token("13762"), spec(None, 13762));
}

#[test]
fn qualified_references_keep_their_repo() {
    assert_eq!(parse_token("example-org/tool-repo#7"), spec(Some("example-org/tool-repo"), 7));
    // Dotted repo names (`.loom`-style) are valid owner/repo charset.
    assert_eq!(parse_token("2AML/test.io#630"), spec(Some("2AML/test.io"), 630));
}

#[test]
fn malformed_tokens_are_carried_verbatim_not_dropped() {
    for bad in ["abc", "owner/#5", "owner/r#x", "-r#5", "0", "-3", ""] {
        assert_eq!(parse_token(bad), ParsedRef::Invalid(bad.to_string()), "{bad:?}");
    }
}

#[test]
fn hash_prefixed_local_refs_parse_like_bare_numbers() {
    // `park_record::BlockerRef` accepts `#N` as an input shape; accepting it
    // here too keeps a future emitter switch from turning every local dep
    // into an eternal UNKNOWN.
    assert_eq!(parse_token("#200"), spec(None, 200));
}

#[test]
fn parse_refs_splits_on_any_whitespace_and_preserves_order() {
    let parsed = parse_refs("200\nexample-org/tool-repo#7  abc");
    assert_eq!(
        parsed,
        vec![
            spec(None, 200),
            spec(Some("example-org/tool-repo"), 7),
            ParsedRef::Invalid("abc".into())
        ]
    );
    assert!(parse_refs("").is_empty());
}

#[test]
fn reference_renders_like_the_announcement_always_did() {
    assert_eq!(
        RefSpec {
            repo: None,
            number: 200
        }
        .reference(),
        "#200"
    );
    assert_eq!(
        RefSpec {
            repo: Some("example-org/tool-repo".into()),
            number: 7
        }
        .reference(),
        "example-org/tool-repo#7"
    );
}

#[test]
fn url_binding_accepts_issue_pull_and_pulls_paths() {
    for url in [
        "https://github.com/example-org/other/issues/630",
        "https://github.com/example-org/other/pull/630",
        "https://gitea.internal/example-org/other/pulls/630",
    ] {
        assert!(url_names_ref(url, "example-org/other", 630), "{url}");
    }
}

#[test]
fn url_binding_is_case_insensitive_on_the_repo() {
    assert!(url_names_ref(
        "https://github.com/2AMLogic/.loom/pull/630",
        "2amlogic/.LOOM",
        630
    ));
}

#[test]
fn url_binding_rejects_the_wrong_repo_at_the_same_number() {
    // The #14710 shape: the answer is for 2AMLogic/2am#630 (an issue),
    // the question was 2AMLogic/.loom#630 (a draft PR).
    assert!(!url_names_ref(
        "https://github.com/2AMLogic/2am/issues/630",
        "2AMLogic/.loom",
        630
    ));
}

#[test]
fn url_binding_rejects_wrong_numbers_paths_and_prefixes() {
    let repo = "example-org/other";
    assert!(!url_names_ref("https://github.com/example-org/other/issues/631", repo, 630));
    assert!(!url_names_ref(
        "https://github.com/example-org/other/issues/630/files",
        repo,
        630
    ));
    assert!(!url_names_ref("https://github.com/example-org/other/projects/630", repo, 630));
    // A longer path whose TAIL is not the node — rfind must not let a prefix match.
    assert!(!url_names_ref("https://github.com/x/example-org/other/issues/999", repo, 630));
    assert!(!url_names_ref("", repo, 630));
}

#[test]
fn state_mapping_fails_safe_on_the_unrecognized() {
    assert_eq!(DepState::from_forge_state("OPEN"), DepState::Open);
    assert_eq!(DepState::from_forge_state("CLOSED"), DepState::Resolved);
    assert_eq!(DepState::from_forge_state("MERGED"), DepState::Resolved);
    for unknown in ["", "DRAFT", "merged", "unknown"] {
        assert_eq!(DepState::from_forge_state(unknown), DepState::Unknown, "{unknown:?}");
    }
}
